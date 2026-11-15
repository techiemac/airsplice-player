//! The playback engine.
//!
//! Threads:
//! - demux: reads packets, decodes audio into the device queue, forwards
//!   video packets to the video thread.
//! - video: decodes video packets into a small PTS-ordered frame queue.
//! - audio device callback: drains the audio queue and advances the clock.
//!
//! Seeking is driven by a generation counter. The UI bumps it and resets the
//! clock immediately; every sample or frame carries the generation it was
//! decoded for and is dropped if stale. No thread ever has to acknowledge a
//! seek, so a seek can never deadlock against a full queue while paused.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration as StdDuration;

use anyhow::{Context as _, Result, anyhow, bail};
use symphonia::core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, Track, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTag};
use symphonia::core::units::{Time, TimeBase, Timestamp};

use super::audio::{AudioOutput, Clock, Converter};
use super::opus::{OPUS_RATE, Opus};
use super::picture::{self, Outline};
use super::upscale::{self, Stage};
use super::video::{self, DecodedFrame, VideoDecoder};

/// Decoded frames kept ahead of the clock.
const MAX_QUEUED_FRAMES: usize = 8;
/// Frames a background player keeps decoded ahead: enough to start at once
/// when brought forward, without holding a full queue (a 4K frame is 33 MB).
const BACKGROUND_QUEUED_FRAMES: usize = 1;
/// Video packets buffered between the demuxer and the video decoder.
const VIDEO_PACKET_QUEUE: usize = 96;
const NO_GENERATION: u64 = u64::MAX;
/// Pre-roll frames this far before a seek target count as "at" the target.
const PREROLL_TOLERANCE: f64 = 0.1;
/// Timestamps this close count as the same instant (container rounding).
const SAME_TIME: f64 = 0.002;
/// Longest the clock waits for the first video frame after open or seek.
const MAX_PREROLL: StdDuration = StdDuration::from_millis(500);

#[derive(Clone, Debug, Default)]
pub struct MediaInfo {
    pub path: PathBuf,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<f64>,
    pub audio: Option<AudioInfo>,
    pub video: Option<VideoInfo>,
    /// Set when the file has a video track we cannot decode.
    pub video_unsupported: Option<String>,
    pub cover: Option<Image>,
}

#[derive(Clone, Debug)]
pub struct AudioInfo {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: usize,
    pub output_rate: u32,
}

#[derive(Clone, Debug)]
pub struct VideoInfo {
    pub codec: &'static str,
    /// The decoder in use, e.g. "VideoToolbox (hardware)".
    pub decoder: &'static str,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub bgra: Arc<Vec<u8>>,
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Image({}x{})", self.width, self.height)
    }
}

pub struct VideoFrame {
    pub pts: f64,
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

struct Shared {
    generation: AtomicU64,
    seek_target: Mutex<f64>,
    frames: Mutex<VecDeque<VideoFrame>>,
    frames_cv: Condvar,
    demux_done: AtomicU64,
    video_done: AtomicU64,
    error: Mutex<Option<String>>,
    stop: AtomicBool,
    /// Largest size video can be shown at, `width << 32 | height`; 0 = unknown.
    output_limit: AtomicU64,
    /// Decoded frames to keep queued ahead of the one on screen.
    max_frames: std::sync::atomic::AtomicUsize,
    /// Upscale frames on the GPU to the output limit (the settings switch).
    upscale: AtomicBool,
    upscale_status: Mutex<upscale::Status>,
}

impl Shared {
    fn current(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

enum VideoMsg {
    Packet { generation: u64, pts: i64, data: Box<[u8]> },
    Reset { discard_before: i64 },
    EndOfStream { generation: u64 },
}

/// Corrects the timestamps Symphonia reports to presentation times:
/// MP4 composition offsets and edit lists (which it ignores), and an encoder
/// delay a track declares but neither trims nor folds into its timestamps
/// (Ogg Opus pre-skip).
struct Timestamps {
    mp4: HashMap<u32, super::mp4_timing::TrackTiming>,
    /// (audio track, declared delay); the shift is decided on its first packet.
    declared_delay: Option<(u32, i64)>,
    delay_shift: Option<i64>,
}

impl Timestamps {
    fn fix(&mut self, p: &mut symphonia::core::packet::Packet) {
        if let Some(t) = self.mp4.get(&p.track_id) {
            p.pts = Timestamp::new(t.pts(p.pts.get()));
            return;
        }
        if let Some((track, delay)) = self.declared_delay
            && track == p.track_id
        {
            // MP3 and Vorbis express the delay as a negative first PTS with
            // `trim_start`; only shift when the packets ignore it.
            let shift = *self.delay_shift.get_or_insert(if p.pts.get() >= 0 && p.trim_start.is_zero() { -delay } else { 0 });
            p.pts = Timestamp::new(p.pts.get() + shift);
        }
    }
}

/// Where the demuxer sends video packets.
struct VideoRoute {
    track_id: u32,
    time_base: TimeBase,
    is_keyframe: video::KeyframeCheck,
    tx: SyncSender<VideoMsg>,
}

fn open_format(path: &Path) -> Result<Box<dyn FormatReader>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| anyhow!("unrecognised media format: {e}"))
}

/// Seek, reopening the file first when the reader is exhausted or the seek
/// fails. Symphonia 0.6.1's MP4 and Matroska readers cannot seek once they
/// have reached the end of the stream.
fn seek_reader(
    format: &mut Box<dyn FormatReader>,
    path: &Path,
    exhausted: &mut bool,
    secs: f64,
    track_id: Option<u32>,
) {
    let to = || SeekTo::Time { time: Time::try_from_secs_f64(secs).unwrap_or_default(), track_id };
    if !*exhausted && format.seek(SeekMode::Accurate, to()).is_ok() {
        return;
    }
    match open_format(path) {
        Ok(fresh) => {
            *format = fresh;
            *exhausted = false;
            if let Err(e) = format.seek(SeekMode::Accurate, to()) {
                log::warn!("seek to {secs:.2}s failed: {e}");
            }
        }
        Err(e) => log::warn!("reopening for seek failed: {e}"),
    }
}

/// Seek so the first video packet read is the latest keyframe at or before
/// the target. Containers differ: Symphonia's MP4 reader ignores sync
/// samples (it lands on the first packet at or after the requested time,
/// often just past a keyframe) and its Matroska reader can land on the next
/// cue point, after the target. So this checks the result rather than
/// trusting either: from where the reader lands, it reads forward through
/// the keyframes up to the target, and backs off 1 s, 2 s, 4 s... only when
/// none comes before the target. Video packets ahead of the chosen keyframe
/// cannot be decoded and are dropped; audio packets are kept (the audio path
/// skips to the target itself).
/// Returns packets already read, to be processed before reading more.
fn seek_to_keyframe(
    format: &mut Box<dyn FormatReader>,
    path: &Path,
    exhausted: &mut bool,
    target: f64,
    seek_track: Option<u32>,
    video: Option<&VideoRoute>,
    timestamps: &mut Timestamps,
) -> VecDeque<symphonia::core::packet::Packet> {
    // In decode order, every keyframe at or before the target comes before
    // any packet shown after the target. Without a keyframe yet, scan a
    // little further in case the reader's timestamps are slightly off.
    const SCAN_PAST_TARGET: f64 = 0.5;
    const MAX_PACKETS: usize = 4096;
    let mut back = 0.0;
    loop {
        let start = (target - back).max(0.0);
        seek_reader(format, path, exhausted, start, seek_track);
        let Some(v) = video else { return VecDeque::new() };
        let mut pending = VecDeque::new();
        let mut key = None;
        let mut skipped = 0;
        let ended = loop {
            let mut p = match format.next_packet() {
                Ok(Some(p)) => p,
                // End of stream or an error. The reader is reopened before
                // the next seek; from the start, the main loop handles it.
                _ => {
                    *exhausted = true;
                    break true;
                }
            };
            timestamps.fix(&mut p);
            if p.track_id != v.track_id {
                pending.push_back(p);
                if pending.len() > MAX_PACKETS {
                    break false;
                }
                continue;
            }
            let at = v.time_base.calc_time(p.pts).map_or(0.0, |t| t.as_secs_f64());
            let is_key = (v.is_keyframe)(&p.data);
            if is_key && at <= target + 0.001 {
                // A keyframe nearer the target: decoding starts here instead.
                let before = pending.len();
                pending.retain(|q| q.track_id != v.track_id);
                skipped += before - pending.len();
                key = Some(at);
                pending.push_back(p);
            } else if key.is_some() {
                pending.push_back(p);
                if at > target + 0.001 || pending.len() > MAX_PACKETS {
                    break false;
                }
            } else if is_key || at > target + SCAN_PAST_TARGET {
                // The next keyframe is after the target: back off.
                pending.push_back(p);
                break false;
            } else {
                // Before any keyframe: undecodable.
                skipped += 1;
            }
        };
        match key {
            Some(at) => {
                log::debug!("seek to {target:.3}: keyframe at {at:.3} (reader asked for {start:.3}, {skipped} video packets skipped)");
                return pending;
            }
            None if start > 0.0 => back = if back == 0.0 { 1.0 } else { back * 2.0 },
            None => {
                log::debug!("seek to {target:.3}: no keyframe before it (ended: {ended}); starting from the beginning");
                return pending;
            }
        }
    }
}

pub struct Player {
    pub info: MediaInfo,
    shared: Arc<Shared>,
    clock: Arc<Clock>,
    audio: Option<AudioOutput>,
    paused: bool,
    /// (generation, pts) of the last frame handed to the UI.
    last_shown: std::cell::Cell<Option<(u64, f64)>>,
    /// When the current pre-roll began (open or seek).
    preroll_since: std::cell::Cell<std::time::Instant>,
    /// Set when playing a still image or an animated GIF.
    picture: Option<Outline>,
    /// How long a still shows before it counts as finished; `None` keeps
    /// it up until the user moves on.
    still_secs: Option<f64>,
}

impl Shared {
    fn new() -> Self {
        Shared {
            generation: AtomicU64::new(0),
            seek_target: Mutex::new(0.0),
            frames: Mutex::new(VecDeque::new()),
            frames_cv: Condvar::new(),
            demux_done: AtomicU64::new(NO_GENERATION),
            video_done: AtomicU64::new(NO_GENERATION),
            error: Mutex::new(None),
            stop: AtomicBool::new(false),
            output_limit: AtomicU64::new(0),
            max_frames: std::sync::atomic::AtomicUsize::new(MAX_QUEUED_FRAMES),
            upscale: AtomicBool::new(false),
            upscale_status: Mutex::new(upscale::Status::Off),
        }
    }
}

impl Player {
    pub fn open(path: &Path, volume: f32, muted: bool) -> Result<Self> {
        if picture::is_picture(path) {
            return Self::open_picture(path);
        }
        let mut format = open_format(path)?;

        let mut info = MediaInfo {
            path: path.to_path_buf(),
            title: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            ..Default::default()
        };
        read_metadata(format.as_mut(), &mut info);

        let audio_track = format.default_track(TrackType::Audio).cloned();
        let mut video_track = format.default_track(TrackType::Video).cloned();

        // Video decoder first: an unsupported codec degrades to audio-only.
        let mut video_decoder: Option<Box<dyn VideoDecoder>> = None;
        if let Some(track) = &video_track {
            let params = track.codec_params.as_ref().and_then(|p| p.video());
            match params.map(video::make_decoder) {
                Some(Ok(dec)) => {
                    let p = params.expect("checked above");
                    info.video = Some(VideoInfo {
                        codec: video::codec_name(p.codec),
                        decoder: dec.name(),
                        width: p.width.unwrap_or(0) as u32,
                        height: p.height.unwrap_or(0) as u32,
                    });
                    video_decoder = Some(dec);
                }
                Some(Err(e)) => info.video_unsupported = Some(e.to_string()),
                None => info.video_unsupported = Some("video track has no codec parameters".into()),
            }
        }
        if video_decoder.is_none() {
            video_track = None;
        }

        let shared = Arc::new(Shared::new());

        let mut audio_out = None;
        let mut audio_pipeline = None;
        if let Some(track) = &audio_track {
            let params = track
                .codec_params
                .as_ref()
                .and_then(|p| p.audio())
                .ok_or_else(|| anyhow!("audio track has no codec parameters"))?;
            let decoder = AudioDec::new(params)?;
            let (src_rate, src_channels) = decoder.format(params)?;
            let out = AudioOutput::open(volume, muted).context("audio output unavailable")?;
            info.audio = Some(AudioInfo {
                codec: decoder.name(),
                sample_rate: src_rate,
                channels: src_channels,
                output_rate: out.rate,
            });
            let converter = Converter::new(src_rate, src_channels, out.rate, out.channels)?;
            audio_pipeline = Some(AudioPipeline {
                track_id: track.id,
                time_base: track.time_base.unwrap_or_else(|| TimeBase::try_from_recip(src_rate).expect("non-zero rate")),
                rate: src_rate,
                decoder,
                converter,
                state: out.state.clone(),
                capacity: out.capacity_samples(),
                src_channels,
                scratch: Vec::new(),
                out: Vec::new(),
            });
            audio_out = Some(out);
        }

        if audio_pipeline.is_none() && video_decoder.is_none() {
            match info.video_unsupported.take() {
                Some(reason) => bail!("{reason}"),
                None => bail!("no playable audio or video track"),
            }
        }

        info.duration = media_duration(format.as_ref());

        let clock = Arc::new(match &audio_out {
            Some(out) => {
                let preroll = video_track.is_some();
                out.state.lock().unwrap().primed = !preroll;
                Clock::Audio { state: out.state.clone(), rate: out.rate, preroll }
            }
            None => Clock::wall(),
        });

        let mut video_route = None;
        if let (Some(decoder), Some(track)) = (video_decoder, &video_track) {
            let params = track.codec_params.as_ref().and_then(|p| p.video()).expect("checked when opening");
            let (tx, rx) = mpsc::sync_channel(VIDEO_PACKET_QUEUE);
            let time_base = track.time_base.unwrap_or_default();
            let shared = shared.clone();
            thread::Builder::new()
                .name("video-decode".into())
                .spawn(move || video_thread(decoder, rx, time_base, shared))?;
            video_route =
                Some(VideoRoute { track_id: track.id, time_base, is_keyframe: video::keyframe_check(params)?, tx });
        }

        let seek_track = video_track.as_ref().or(audio_track.as_ref()).map(|t| t.id);
        let timestamps = Timestamps {
            mp4: if format.format_info().short_name == "isomp4" { super::mp4_timing::read(path) } else { HashMap::new() },
            declared_delay: audio_track.as_ref().and_then(|t| Some((t.id, t.delay.filter(|d| *d > 0)? as i64))),
            delay_shift: None,
        };
        {
            let shared = shared.clone();
            let path = path.to_path_buf();
            thread::Builder::new()
                .name("demux".into())
                .spawn(move || demux_thread(format, path, audio_pipeline, video_route, seek_track, timestamps, shared))?;
        }

        Ok(Self {
            info,
            shared,
            clock,
            audio: audio_out,
            paused: false,
            last_shown: Default::default(),
            preroll_since: std::cell::Cell::new(std::time::Instant::now()),
            picture: None,
            still_secs: None,
        })
    }

    /// A still image or animated GIF: frames come from a picture thread
    /// instead of a demuxer and decoder, timed by the wall clock.
    fn open_picture(path: &Path) -> Result<Self> {
        let outline = picture::outline(path)?;
        let info = MediaInfo {
            path: path.to_path_buf(),
            title: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            duration: outline.duration,
            video: Some(VideoInfo { codec: outline.format, decoder: "image (software)", width: outline.width, height: outline.height }),
            ..Default::default()
        };
        let shared = Arc::new(Shared::new());
        {
            let (shared, path, outline) = (shared.clone(), path.to_path_buf(), outline.clone());
            thread::Builder::new().name("picture".into()).spawn(move || picture_thread(&path, &outline, &shared))?;
        }
        Ok(Self {
            info,
            shared,
            clock: Arc::new(Clock::wall()),
            audio: None,
            paused: false,
            last_shown: Default::default(),
            preroll_since: std::cell::Cell::new(std::time::Instant::now()),
            picture: Some(outline),
            still_secs: None,
        })
    }

    /// How long a still image shows (`None`: until the user moves on). A
    /// still with a duration has a timeline like a clip of that length.
    /// No effect on anything but stills.
    pub fn set_still_duration(&mut self, secs: Option<f64>) {
        if self.picture.as_ref().is_some_and(Outline::is_still) {
            self.still_secs = secs.filter(|s| *s > 0.0);
            self.info.duration = self.still_secs;
        }
    }

    pub fn position(&self) -> f64 {
        // A still without a duration has no timeline.
        if self.picture.as_ref().is_some_and(Outline::is_still) && self.still_secs.is_none() {
            return 0.0;
        }
        let pos = self.clock.now();
        match self.info.duration {
            Some(d) => pos.clamp(0.0, d),
            None => pos.max(0.0),
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        self.clock.set_paused(paused);
    }

    pub fn seek(&mut self, secs: f64) {
        let secs = match self.info.duration {
            Some(d) => secs.clamp(0.0, (d - 0.05).max(0.0)),
            None => secs.max(0.0),
        };
        let generation = self.shared.current() + 1;
        self.preroll_since.set(std::time::Instant::now());
        // Order matters: invalidate queued audio and frames under their
        // locks before publishing the new generation.
        self.clock.seek(secs, generation);
        *self.shared.seek_target.lock().unwrap() = secs;
        self.shared.frames.lock().unwrap().clear();
        self.shared.generation.store(generation, Ordering::Release);
        self.shared.frames_cv.notify_all();
    }

    pub fn set_volume(&self, volume: f32, muted: bool) {
        if let Some(out) = &self.audio {
            let mut st = out.state.lock().unwrap();
            st.volume = volume;
            st.muted = muted;
        }
    }

    /// A background player (a tab not on screen) keeps only the frame it is
    /// cued on and one more, instead of a full queue. Going to the
    /// background while paused re-cues at the current position, which drops
    /// frames queued beyond that.
    pub fn set_background(&mut self, background: bool) {
        let limit = if background { BACKGROUND_QUEUED_FRAMES } else { MAX_QUEUED_FRAMES };
        if self.shared.max_frames.swap(limit, Ordering::Relaxed) == limit {
            return;
        }
        if background && self.paused && self.shared.frames.lock().unwrap().len() > limit {
            self.seek(self.position());
        }
        self.shared.frames_cv.notify_all();
    }

    /// Tell the decoder the largest size video can currently be shown at.
    pub fn set_output_limit(&self, width: u32, height: u32) {
        self.shared.output_limit.store((width as u64) << 32 | height as u64, Ordering::Relaxed);
    }

    /// Upscale video on the GPU to the output limit. Applies to frames
    /// decoded from now on.
    pub fn set_upscaling(&self, on: bool) {
        self.shared.upscale.store(on, Ordering::Relaxed);
    }

    /// What upscaling did with the latest decoded frame (so, while paused,
    /// with the frame on screen).
    pub fn upscale_status(&self) -> upscale::Status {
        self.shared.upscale_status.lock().unwrap().clone()
    }

    pub fn levels(&self) -> [f32; 2] {
        self.audio.as_ref().map_or([0.0; 2], |o| o.state.lock().unwrap().peak)
    }

    /// The newest decoded frame whose time has come, dropping any older ones.
    pub fn take_frame(&self) -> Option<VideoFrame> {
        let generation = self.shared.current();
        let target = *self.shared.seek_target.lock().unwrap();
        if !self.clock.is_primed() {
            // Pre-roll: hold the clock until the frame for the start point
            // is decoded (bounded, so a broken stream cannot hang playback).
            let ready = self
                .shared
                .frames
                .lock()
                .unwrap()
                .iter()
                .any(|f| f.generation == generation && f.pts >= target - PREROLL_TOLERANCE);
            let gave_up = self.preroll_since.get().elapsed() > MAX_PREROLL
                || self.shared.video_done.load(Ordering::Acquire) == generation;
            if ready || gave_up {
                self.clock.prime();
            }
        }
        let now = self.clock.now();
        let mut frames = self.shared.frames.lock().unwrap();
        let mut best = None;
        let last_shown = self.last_shown.get().filter(|(g, _)| *g == generation).map(|(_, pts)| pts);
        while let Some(front) = frames.front() {
            if front.generation != generation || last_shown.is_some_and(|l| front.pts <= l) {
                // Stale, or older than what is on screen: a frame that
                // arrived late must never make the picture step backwards.
                frames.pop_front();
            } else if front.pts <= now + 0.004 {
                best = frames.pop_front();
            } else {
                break;
            }
        }
        // Right after a seek the decoder works through frames from the
        // previous keyframe. Show one from before the target only once it is
        // provably the frame on screen at the target (its successor is past
        // the target, or there is no successor); otherwise hold it.
        let video_ended = self.shared.video_done.load(Ordering::Acquire) == generation;
        if let Some(f) = best.take_if(|f| {
            f.pts < target - SAME_TIME && !video_ended && !frames.front().is_some_and(|next| next.pts > target)
        }) {
            frames.push_front(f);
        }
        // Nothing shown since the open or seek yet: show the first frame at
        // the target even if its time has not quite come. Video often starts
        // a little after time zero (B-frames without an edit list), and a
        // paused clock would otherwise never reach it.
        if best.is_none()
            && last_shown.is_none()
            && frames.front().is_some_and(|f| f.generation == generation && f.pts >= target - SAME_TIME)
        {
            best = frames.pop_front();
        }
        drop(frames);
        self.shared.frames_cv.notify_all();
        if let Some(f) = &best {
            self.last_shown.set(Some((generation, f.pts)));
        }
        best
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    pub fn is_finished(&self) -> bool {
        match &self.picture {
            // A still shows for its set duration, or until the user moves on.
            Some(p) if p.is_still() => return self.still_secs.is_some_and(|d| self.clock.now() >= d - SAME_TIME),
            // An animation's last frame shows for its full delay.
            Some(p) if self.clock.now() < p.duration.unwrap_or(0.0) - SAME_TIME => return false,
            _ => {}
        }
        let generation = self.shared.current();
        if self.shared.demux_done.load(Ordering::Acquire) != generation {
            return false;
        }
        let audio_drained = self.audio.as_ref().is_none_or(|o| o.state.lock().unwrap().buf.is_empty());
        let video_drained = self.info.video.is_none()
            || (self.shared.video_done.load(Ordering::Acquire) == generation
                && self.shared.frames.lock().unwrap().is_empty());
        audio_drained && video_drained
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.frames_cv.notify_all();
    }
}

/// The container's own duration (Matroska carries it only at segment
/// level), else the longest audio or video track.
fn media_duration(format: &dyn FormatReader) -> Option<f64> {
    let media = format.media_info();
    media.time_base.zip(media.duration).and_then(|(tb, d)| tb.calc_duration(d)).map(|t| t.as_secs_f64()).or_else(|| {
        [format.default_track(TrackType::Audio), format.default_track(TrackType::Video)]
            .into_iter()
            .flatten()
            .filter_map(track_duration)
            .reduce(f64::max)
    })
}

/// What the playlist shows for a file, read without decoding anything.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub duration: Option<f64>,
    pub has_video: bool,
}

pub fn probe(path: &Path) -> Result<Summary> {
    if picture::is_picture(path) {
        let outline = picture::outline(path)?;
        return Ok(Summary { title: None, artist: None, duration: outline.duration, has_video: true });
    }
    let mut format = open_format(path)?;
    let mut info = MediaInfo::default();
    read_tags(format.as_mut(), &mut info, false);
    Ok(Summary {
        title: (!info.title.is_empty()).then_some(info.title),
        artist: info.artist,
        duration: media_duration(format.as_ref()),
        has_video: format.default_track(TrackType::Video).is_some(),
    })
}

fn track_duration(track: &Track) -> Option<f64> {
    let tb = track.time_base?;
    let dur = track.duration.or_else(|| track.num_frames.map(Into::into))?;
    Some(tb.calc_duration(dur)?.as_secs_f64())
}

fn read_metadata(format: &mut dyn FormatReader, info: &mut MediaInfo) {
    read_tags(format, info, true);
}

fn read_tags(format: &mut dyn FormatReader, info: &mut MediaInfo, with_cover: bool) {
    let mut metadata = format.metadata();
    let Some(rev) = metadata.skip_to_latest() else { return };
    for tag in &rev.media.tags {
        match &tag.std {
            Some(StandardTag::TrackTitle(s)) if !s.trim().is_empty() => info.title = s.to_string(),
            Some(StandardTag::Artist(s)) => info.artist = Some(s.to_string()),
            Some(StandardTag::Album(s)) => info.album = Some(s.to_string()),
            _ => {}
        }
    }
    if with_cover {
        info.cover = rev.media.visuals.first().and_then(|v| decode_image(&v.data));
    }
}

pub fn decode_image(bytes: &[u8]) -> Option<Image> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (width, height) = img.dimensions();
    let mut bgra = img.into_raw();
    for px in bgra.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    Some(Image { width, height, bgra: Arc::new(bgra) })
}

/// Symphonia's decoders plus the Opus decoder it lacks.
enum AudioDec {
    Symphonia(Box<dyn AudioDecoder>),
    Opus(Box<Opus>),
}

impl AudioDec {
    fn new(params: &AudioCodecParameters) -> Result<Self> {
        if params.codec == CODEC_ID_OPUS {
            let hint = params.channels.as_ref().map_or(2, |c| c.count());
            return Ok(AudioDec::Opus(Box::new(Opus::new(params.extra_data.as_deref(), hint)?)));
        }
        symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map(AudioDec::Symphonia)
            .map_err(|e| anyhow!("unsupported audio codec: {e}"))
    }

    /// Decoded sample rate and channel count.
    fn format(&self, params: &AudioCodecParameters) -> Result<(u32, usize)> {
        match self {
            AudioDec::Opus(o) => Ok((OPUS_RATE, o.channels())),
            AudioDec::Symphonia(_) => Ok((
                params.sample_rate.ok_or_else(|| anyhow!("audio track has no sample rate"))?,
                params.channels.as_ref().map_or(2, |c| c.count()),
            )),
        }
    }

    fn name(&self) -> String {
        match self {
            AudioDec::Opus(_) => "Opus".into(),
            AudioDec::Symphonia(d) => d.codec_info().short_name.to_uppercase(),
        }
    }

    fn reset(&mut self) {
        match self {
            AudioDec::Opus(o) => o.reset(),
            AudioDec::Symphonia(d) => d.reset(),
        }
    }

    /// Decode into interleaved f32; returns the frame count.
    fn decode(&mut self, packet: &symphonia::core::packet::Packet, out: &mut Vec<f32>) -> Result<usize> {
        match self {
            AudioDec::Opus(o) => o.decode(&packet.data, out),
            AudioDec::Symphonia(d) => {
                let buf = d.decode(packet).map_err(|e| anyhow!("{e}"))?;
                out.resize(buf.samples_interleaved(), 0.0);
                buf.copy_to_slice_interleaved(&mut *out);
                Ok(buf.frames())
            }
        }
    }
}

impl AudioPipeline {
    /// Every sample of this generation has been decoded and queued.
    fn mark_ended(&self, generation: u64) {
        let mut st = self.state.lock().unwrap();
        if st.generation == generation {
            st.ended = true;
        }
    }
}

struct AudioPipeline {
    track_id: u32,
    time_base: TimeBase,
    rate: u32,
    decoder: AudioDec,
    converter: Converter,
    state: Arc<Mutex<super::audio::AudioState>>,
    capacity: usize,
    src_channels: usize,
    scratch: Vec<f32>,
    out: Vec<f32>,
}

impl AudioPipeline {
    fn reset(&mut self) {
        self.decoder.reset();
        self.converter.reset();
    }

    /// Decode one packet and queue it for the device, blocking while the
    /// queue is full. `skip_until` drops audio before a seek target.
    fn handle(&mut self, packet: &symphonia::core::packet::Packet, generation: u64, skip_until: &mut Option<f64>, shared: &Shared) {
        let frames = match self.decoder.decode(packet, &mut self.scratch) {
            Ok(n) => n,
            Err(e) => {
                // Corrupt packets are skipped, as every player does.
                log::debug!("audio decode error: {e}");
                return;
            }
        };
        let ch = self.src_channels.max(1);
        let frames = frames.min(self.scratch.len() / ch);
        if frames == 0 {
            return;
        }

        // Encoder delay and padding (gapless playback).
        let to_frames = |d: symphonia::core::units::Duration| {
            self.time_base.calc_duration(d).map_or(0, |t| (t.as_secs_f64() * self.rate as f64).round() as usize)
        };
        let mut start = to_frames(packet.trim_start).min(frames);
        let end = frames - to_frames(packet.trim_end).min(frames - start);

        if let Some(target) = *skip_until {
            // `pts` marks the first decoded frame, before `trim_start`.
            let pkt_start = self.time_base.calc_time(packet.pts).map_or(0.0, |t| t.as_secs_f64()) + start as f64 / self.rate as f64;
            let pkt_end = pkt_start + (end - start) as f64 / self.rate as f64;
            if pkt_end <= target {
                return;
            }
            start += (((target - pkt_start) * self.rate as f64).max(0.0) as usize).min(end - start);
            *skip_until = None;
        }

        self.out.clear();
        self.converter.push(&self.scratch[start * ch..end * ch], &mut self.out);
        if self.out.is_empty() {
            return;
        }
        loop {
            {
                let mut st = self.state.lock().unwrap();
                if st.generation != generation {
                    return;
                }
                if st.buf.is_empty() || st.buf.len() + self.out.len() <= self.capacity {
                    st.buf.extend(self.out.iter().copied());
                    return;
                }
            }
            if shared.stopped() || shared.current() != generation {
                return;
            }
            // Back-pressure: the device drains ~10 ms per callback.
            thread::sleep(StdDuration::from_millis(5));
        }
    }
}

fn demux_thread(
    mut format: Box<dyn FormatReader>,
    path: PathBuf,
    mut audio: Option<AudioPipeline>,
    video: Option<VideoRoute>,
    seek_track: Option<u32>,
    mut timestamps: Timestamps,
    shared: Arc<Shared>,
) {
    let mut generation = 0;
    let mut eof = false;
    // Audio before time zero is encoder priming: drop it from the start.
    let mut skip_until: Option<f64> = Some(0.0);
    let mut pending = VecDeque::new();
    // Set once the reader has returned end-of-stream or an error.
    let mut exhausted = false;
    loop {
        if shared.stopped() {
            return;
        }
        let current = shared.current();
        if current != generation {
            generation = current;
            let target = *shared.seek_target.lock().unwrap();
            pending = seek_to_keyframe(&mut format, &path, &mut exhausted, target, seek_track, video.as_ref(), &mut timestamps);
            if let Some(a) = audio.as_mut() {
                a.reset();
            }
            if let Some(v) = &video {
                // Frames shortly before the target are still converted so a
                // paused seek can show the frame on screen at the target.
                let margin = Time::try_from_secs_f64((target - 0.25).max(0.0)).unwrap_or_default();
                let discard_before = v.time_base.calc_timestamp(margin).map_or(i64::MIN, |t| t.get());
                // Blocking send is fine: the video thread drops stale
                // packets without decoding, so the queue drains quickly.
                let _ = v.tx.send(VideoMsg::Reset { discard_before });
            }
            skip_until = Some(target);
            eof = false;
        }
        if eof {
            thread::sleep(StdDuration::from_millis(20));
            continue;
        }
        let next = match pending.pop_front() {
            Some(p) => Ok(Some(p)),
            None => format.next_packet().map(|p| {
                p.map(|mut p| {
                    timestamps.fix(&mut p);
                    p
                })
            }),
        };
        let packet = match next {
            Ok(Some(p)) => p,
            Ok(None) => {
                eof = true;
                exhausted = true;
                if let Some(v) = &video {
                    let _ = v.tx.send(VideoMsg::EndOfStream { generation });
                }
                if let Some(a) = &audio {
                    a.mark_ended(generation);
                }
                shared.demux_done.store(generation, Ordering::Release);
                continue;
            }
            Err(SymError::ResetRequired) => {
                if let Some(a) = audio.as_mut() {
                    a.reset();
                }
                continue;
            }
            Err(e) => {
                log::warn!("demux error, treating as end of stream: {e}");
                eof = true;
                exhausted = true;
                if let Some(v) = &video {
                    let _ = v.tx.send(VideoMsg::EndOfStream { generation });
                }
                if let Some(a) = &audio {
                    a.mark_ended(generation);
                }
                shared.demux_done.store(generation, Ordering::Release);
                continue;
            }
        };

        if let Some(a) = audio.as_mut().filter(|a| a.track_id == packet.track_id) {
            a.handle(&packet, generation, &mut skip_until, &shared);
        } else if let Some(v) = video.as_ref().filter(|v| v.track_id == packet.track_id) {
            let mut msg = VideoMsg::Packet { generation, pts: packet.pts.get(), data: packet.data };
            loop {
                match v.tx.try_send(msg) {
                    Ok(()) => break,
                    Err(TrySendError::Full(m)) => {
                        if shared.stopped() || shared.current() != generation {
                            break;
                        }
                        msg = m;
                        thread::sleep(StdDuration::from_millis(2));
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        *shared.error.lock().unwrap() = Some("video decoder stopped".into());
                        break;
                    }
                }
            }
        }
    }
}

/// Queue one decoded frame for display, upscaled first if that is switched
/// on, waiting while the queue is full. False when a seek or stop made the
/// frame moot (the caller stops pushing that generation).
fn queue_frame(shared: &Shared, stage: &mut Stage, generation: u64, pts: f64, width: u32, height: u32, bgra: Vec<u8>) -> bool {
    // Upscaled here, off the UI thread, to the size it is shown at.
    let (width, height, bgra) = if shared.upscale.load(Ordering::Relaxed) {
        let packed = shared.output_limit.load(Ordering::Relaxed);
        let limit = (packed != 0).then_some(((packed >> 32) as u32, packed as u32));
        let (frame, status) = stage.process(pts, width, height, bgra, limit);
        *shared.upscale_status.lock().unwrap() = status;
        frame
    } else {
        *shared.upscale_status.lock().unwrap() = upscale::Status::Off;
        (width, height, bgra)
    };
    let target = *shared.seek_target.lock().unwrap();
    let mut q = shared.frames.lock().unwrap();
    // Of the frames at or before the seek target only the latest can
    // ever be shown (it is the one on screen at the target), so a
    // newer one replaces older ones instead of waiting behind them.
    // With a one-frame queue, waiting would never end.
    let held = |e: &VideoFrame| e.generation == generation && e.pts <= target + SAME_TIME;
    if pts <= target + SAME_TIME {
        q.retain(|e| !held(e) || e.pts >= pts);
    }
    // The one frame held at or before the target does not count: the
    // picker needs its successor to know it is the one to show.
    while q.iter().filter(|e| !held(e)).count() >= shared.max_frames.load(Ordering::Relaxed)
        && shared.current() == generation
        && !shared.stopped()
    {
        q = shared.frames_cv.wait_timeout(q, StdDuration::from_millis(100)).unwrap().0;
    }
    if shared.current() != generation || shared.stopped() {
        return false;
    }
    // Decoders may emit in decode order; keep the queue in PTS order.
    let at = q.iter().position(|e| e.pts > pts).unwrap_or(q.len());
    q.insert(at, VideoFrame { pts, generation, width, height, bgra });
    true
}

/// Feeds a picture's frames into the queue like a video decoder would:
/// from the frame on screen at the seek target onwards, then waits for the
/// next seek. Frames are decoded once, in order, and kept, so scrubbing
/// back is instant.
fn picture_thread(path: &Path, outline: &Outline, shared: &Shared) {
    let mut stage = Stage::default();
    let mut decoded: Vec<Vec<u8>> = Vec::new();
    let mut frames = None;
    loop {
        if shared.stopped() {
            return;
        }
        let generation = shared.current();
        let target = *shared.seek_target.lock().unwrap();
        let first = outline.starts.partition_point(|&s| s <= target + SAME_TIME).saturating_sub(1);
        let mut interrupted = false;
        for (i, &start) in outline.starts.iter().enumerate().skip(first) {
            while decoded.len() <= i {
                let next = match frames.as_mut() {
                    Some(f) => f,
                    None => match picture::frames(path) {
                        Ok(f) => frames.insert(f),
                        Err(e) => {
                            *shared.error.lock().unwrap() = Some(format!("{e:#}"));
                            return;
                        }
                    },
                };
                match next.next() {
                    Some(Ok(px)) => decoded.push(px),
                    Some(Err(e)) => {
                        *shared.error.lock().unwrap() = Some(format!("{e:#}"));
                        return;
                    }
                    // Fewer frames than the outline counted: end here.
                    None => break,
                }
            }
            let Some(px) = decoded.get(i) else { break };
            if !queue_frame(shared, &mut stage, generation, start, outline.width, outline.height, px.clone()) {
                interrupted = true;
                break;
            }
        }
        if interrupted {
            continue;
        }
        shared.video_done.store(generation, Ordering::Release);
        shared.demux_done.store(generation, Ordering::Release);
        let mut q = shared.frames.lock().unwrap();
        while shared.current() == generation && !shared.stopped() {
            q = shared.frames_cv.wait_timeout(q, StdDuration::from_millis(100)).unwrap().0;
        }
    }
}

fn video_thread(mut decoder: Box<dyn VideoDecoder>, rx: Receiver<VideoMsg>, time_base: TimeBase, shared: Arc<Shared>) {
    let mut stage = Stage::default();
    let mut push = |frames: Vec<DecodedFrame>, generation: u64| {
        for f in frames {
            let pts = time_base.calc_time(Timestamp::new(f.pts)).map_or(0.0, |t| t.as_secs_f64());
            if !queue_frame(&shared, &mut stage, generation, pts, f.width, f.height, f.bgra) {
                return;
            }
        }
    };
    let mut applied_limit = 0u64;
    while let Ok(msg) = rx.recv() {
        if shared.stopped() {
            return;
        }
        match msg {
            VideoMsg::Packet { generation, pts, data } => {
                if generation != shared.current() {
                    continue;
                }
                let packed = shared.output_limit.load(Ordering::Relaxed);
                if packed != applied_limit {
                    applied_limit = packed;
                    decoder.set_output_limit((packed != 0).then_some(((packed >> 32) as u32, packed as u32)));
                }
                match decoder.decode(&data, pts) {
                    Ok(frames) => push(frames, generation),
                    Err(e) => log::debug!("video decode error: {e}"),
                }
            }
            VideoMsg::Reset { discard_before } => {
                if let Err(e) = decoder.reset(discard_before) {
                    *shared.error.lock().unwrap() = Some(format!("video decoder reset failed: {e}"));
                }
            }
            VideoMsg::EndOfStream { generation } => {
                if let Ok(frames) = decoder.flush() {
                    push(frames, generation);
                }
                shared.video_done.store(generation, Ordering::Release);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    /// Decode every audio packet of a fixture the way the demux thread does.
    fn decode_audio(name: &str) -> (u32, usize, Vec<f32>, String) {
        let path = fixture(name);
        let mss = MediaSourceStream::new(Box::new(File::open(&path).unwrap()), Default::default());
        let mut hint = Hint::new();
        hint.with_extension(path.extension().unwrap().to_str().unwrap());
        let mut format = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .unwrap();
        let track = format.default_track(TrackType::Audio).unwrap().clone();
        let params = track.codec_params.as_ref().unwrap().audio().unwrap();
        let mut dec = AudioDec::new(params).unwrap();
        let (rate, channels) = dec.format(params).unwrap();
        let name = dec.name();
        let mut all = Vec::new();
        let mut buf = Vec::new();
        while let Some(packet) = format.next_packet().unwrap() {
            if packet.track_id != track.id {
                continue;
            }
            let frames = dec.decode(&packet, &mut buf).unwrap_or_else(|e| panic!("{name}: packet pts {} len {}: {e}", packet.pts, packet.data.len()));
            all.extend_from_slice(&buf[..frames * channels]);
        }
        (rate, channels, all, name)
    }

    /// Estimate the tone frequency of channel `ch` from rising zero crossings.
    fn frequency(samples: &[f32], channels: usize, ch: usize, rate: u32) -> f64 {
        let mono: Vec<f32> = samples.chunks_exact(channels).map(|f| f[ch]).collect();
        let body = &mono[mono.len() / 4..mono.len() * 3 / 4];
        let crossings = body.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        crossings as f64 * rate as f64 / body.len() as f64
    }

    #[test]
    fn surround_aac_puts_each_channel_in_its_place() {
        // Each channel carries its own tone; output is in Symphonia's
        // order (FL, FR, FC, LFE, RL, RR), not AAC's (C, L R, Ls Rs, LFE).
        let (rate, channels, samples, _) = decode_audio("aac_51.m4a");
        assert_eq!((rate, channels), (48_000, 6));
        for (ch, (label, want)) in [("FL", 300.0), ("FR", 500.0), ("FC", 700.0), ("LFE", 80.0), ("RL", 900.0), ("RR", 1100.0)].into_iter().enumerate() {
            let got = frequency(&samples, channels, ch, rate);
            assert!((got - want).abs() < want * 0.03, "{label}: {got:.0} Hz, expected {want} Hz");
        }
    }

    #[test]
    fn every_audio_codec_decodes_a_440_hz_tone() {
        let cases = [
            ("tagged.mp3", 44_100, 2, 2.0, "MP3"),
            ("mono_96k.flac", 96_000, 1, 2.0, "FLAC"),
            ("stereo.opus", 48_000, 2, 2.0, "Opus"),
            ("stereo.ogg", 44_100, 2, 2.0, "VORBIS"),
            ("stereo_22k.wav", 22_050, 2, 2.0, "PCM_S16LE"),
            ("h264_aac.mp4", 44_100, 2, 3.0, "AAC"),
            ("av1_opus.webm", 48_000, 2, 3.0, "Opus"),
            ("av1_10bit_opus51.mkv", 48_000, 6, 3.0, "Opus"),
        ];
        for (name, want_rate, want_ch, secs, codec) in cases {
            let (rate, ch, samples, got_codec) = decode_audio(name);
            assert_eq!((rate, ch), (want_rate, want_ch), "{name}: format");
            assert!(got_codec.contains(codec) || codec.contains(&got_codec), "{name}: codec {got_codec}");
            let dur = samples.len() as f64 / ch as f64 / rate as f64;
            // Decoders emit priming/padding frames; trimming happens later.
            assert!((dur - secs).abs() < 0.1, "{name}: decoded {dur:.3}s, want {secs}s");
            for c in 0..ch {
                let f = frequency(&samples, ch, c, rate);
                assert!((f - 440.0).abs() < 5.0, "{name}: channel {c} at {f:.1} Hz");
            }
            let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            assert!(peak > 0.05 && peak <= 1.01, "{name}: peak {peak}");
        }
    }

    /// Seek the way the demux thread does and return the time of the first
    /// video packet, which must be a keyframe.
    fn first_video_after_seek(name: &str, target: f64) -> f64 {
        let path = fixture(name);
        let mut format = open_format(&path).unwrap();
        let track = format.default_track(TrackType::Video).cloned().unwrap();
        let audio = format.default_track(TrackType::Audio).cloned();
        let params = track.codec_params.as_ref().and_then(|p| p.video()).unwrap();
        let (tx, _rx) = mpsc::sync_channel(1);
        let route = VideoRoute {
            track_id: track.id,
            time_base: track.time_base.unwrap(),
            is_keyframe: video::keyframe_check(params).unwrap(),
            tx,
        };
        let mut timestamps = Timestamps {
            mp4: if format.format_info().short_name == "isomp4" { super::super::mp4_timing::read(&path) } else { HashMap::new() },
            declared_delay: audio.as_ref().and_then(|t| Some((t.id, t.delay.filter(|d| *d > 0)? as i64))),
            delay_shift: None,
        };
        let mut exhausted = false;
        let pending = seek_to_keyframe(&mut format, &path, &mut exhausted, target, Some(track.id), Some(&route), &mut timestamps);
        let first = pending.iter().find(|p| p.track_id == track.id).unwrap_or_else(|| panic!("{name} @ {target}: no video packet"));
        assert!((route.is_keyframe)(&first.data), "{name} @ {target}: first video packet is not a keyframe");
        route.time_base.calc_time(first.pts).unwrap().as_secs_f64()
    }

    #[test]
    fn seeks_start_at_the_latest_keyframe_before_the_target() {
        // Keyframes: seek_hevc.mp4 every 2 s (on whole seconds, where
        // Symphonia's MP4 reader lands one packet past them); the others
        // every second.
        let cases: [(&str, f64, &[f64]); 4] = [
            ("seek_hevc.mp4", 2.0, &[0.5, 2.0, 3.0, 4.0, 5.0, 6.5, 8.0, 9.5]),
            ("h264_aac.mp4", 1.0, &[0.5, 1.0, 1.5, 2.0, 2.7]),
            ("hevc_10bit_opus.mkv", 1.0, &[0.5, 1.0, 1.5, 2.0, 2.7]),
            ("bframes_hevc.mkv", 1.0, &[0.3, 1.0, 1.6]),
        ];
        for (name, interval, targets) in cases {
            for &target in targets {
                let expected = (target / interval + 1e-6).floor() * interval;
                let at = first_video_after_seek(name, target);
                assert!((at - expected).abs() < 0.02, "{name}: seek to {target} started at {at}, expected the keyframe at {expected}");
            }
        }
    }

    /// Wait up to `budget_ms` for the frame the player shows next.
    fn next_frame(p: &Player, budget_ms: u64) -> VideoFrame {
        let deadline = std::time::Instant::now() + StdDuration::from_millis(budget_ms);
        loop {
            if let Some(f) = p.take_frame() {
                return f;
            }
            assert!(std::time::Instant::now() < deadline, "no frame within {budget_ms} ms");
            thread::sleep(StdDuration::from_millis(5));
        }
    }

    #[test]
    fn an_animated_gif_plays_and_scrubs_like_a_video() {
        use crate::media::picture::tests::{animated_gif, colour, temp_dir};
        let dir = temp_dir("player");
        let gif = animated_gif(&dir);
        let bgra = |i: usize| {
            let [r, g, b, a] = colour(i);
            [b, g, r, a]
        };
        let mut p = Player::open(&gif, 0.0, true).unwrap();
        assert_eq!(p.info.duration, Some(1.0));
        let v = p.info.video.as_ref().unwrap();
        assert_eq!((v.codec, v.width, v.height), ("GIF", 32, 24));
        assert_eq!(next_frame(&p, 2000).bgra[..4], bgra(0), "first frame");

        // Paused scrubbing: each target shows the frame on screen then
        // (frames start at 0, 0.1, 0.3 and 0.6 s), backwards included.
        p.set_paused(true);
        for (target, frame) in [(0.35, 2), (0.65, 3), (0.05, 0), (0.15, 1), (0.6, 3)] {
            p.seek(target);
            let f = next_frame(&p, 2000);
            assert_eq!(f.bgra[..4], bgra(frame), "seek to {target}s showed the wrong frame (pts {})", f.pts);
            assert!((p.position() - target).abs() < 1e-9);
        }

        // Played from 0.6 s: the last frame holds for its whole 400 ms, then
        // the clip ends.
        p.set_paused(false);
        let started = std::time::Instant::now();
        while !p.is_finished() {
            assert!(started.elapsed() < StdDuration::from_secs(3), "never finished");
            let _ = p.take_frame();
            thread::sleep(StdDuration::from_millis(5));
        }
        assert!(started.elapsed() >= StdDuration::from_millis(350), "ended after {:?}, before the last frame's delay", started.elapsed());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_still_image_shows_and_stays() {
        use crate::media::picture::tests::temp_dir;
        let dir = temp_dir("still");
        let path = dir.join("still.png");
        image::RgbaImage::from_pixel(20, 10, image::Rgba([250, 100, 0, 255])).save(&path).unwrap();
        let p = Player::open(&path, 0.0, true).unwrap();
        assert_eq!(p.info.duration, None);
        assert_eq!(p.info.video.as_ref().map(|v| v.codec), Some("PNG"));
        let f = next_frame(&p, 2000);
        assert_eq!((f.width, f.height, &f.bgra[..4]), (20, 10, &[0, 100, 250, 255][..]));
        // Nothing ends it and its clock does not run.
        let until = std::time::Instant::now() + StdDuration::from_millis(300);
        while std::time::Instant::now() < until {
            assert!(!p.is_finished(), "a still finished on its own");
            assert_eq!(p.position(), 0.0);
            thread::sleep(StdDuration::from_millis(5));
        }
        assert!(probe(&path).is_ok_and(|s| s.has_video && s.duration.is_none()));

        // Given a duration, a still is a clip of that length: its clock runs
        // and it finishes, so a playlist moves on.
        let mut p = Player::open(&path, 0.0, true).unwrap();
        p.set_still_duration(Some(0.3));
        assert_eq!(p.info.duration, Some(0.3));
        let _ = next_frame(&p, 2000);
        let started = std::time::Instant::now();
        while !p.is_finished() {
            assert!(started.elapsed() < StdDuration::from_secs(3), "never finished");
            thread::sleep(StdDuration::from_millis(5));
        }
        assert!(p.position() > 0.25, "finished at {}", p.position());
        // Back to "until skipped".
        p.set_still_duration(None);
        assert_eq!((p.info.duration, p.position(), p.is_finished()), (None, 0.0, false));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_background_player_keeps_one_frame_queued_and_stays_cued() {
        let queued = |p: &Player| p.shared.frames.lock().unwrap().len();
        let within = |what: &str, budget_ms: u64, mut done: Box<dyn FnMut() -> bool + '_>| {
            let deadline = std::time::Instant::now() + StdDuration::from_millis(budget_ms);
            while !done() {
                assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
                thread::sleep(StdDuration::from_millis(5));
            }
        };
        // Video only, so no audio device is needed.
        let mut p = Player::open(&fixture("video_only.mp4"), 0.0, true).unwrap();
        p.set_paused(true);
        p.seek(1.2);
        within("a full queue", 3000, Box::new(|| queued(&p) >= MAX_QUEUED_FRAMES));

        // To the background: re-cued at the same point with one frame queued.
        p.set_background(true);
        let mut cued = None;
        within("the cued frame", 3000, Box::new(|| {
            cued = cued.take().or_else(|| p.take_frame());
            cued.is_some()
        }));
        let f = cued.unwrap();
        assert!((f.pts - 1.2).abs() < 0.05, "cued on {:.3}s, not 1.2s", f.pts);
        assert!((p.position() - 1.2).abs() < 1e-9, "position moved to {}", p.position());
        let watch_until = std::time::Instant::now() + StdDuration::from_millis(300);
        while std::time::Instant::now() < watch_until {
            assert!(queued(&p) <= BACKGROUND_QUEUED_FRAMES, "{} frames queued in the background", queued(&p));
            thread::sleep(StdDuration::from_millis(5));
        }

        // A target between frames (1.21 s) shows the frame on screen then
        // (1.20 s). Knowing that needs the next frame decoded, which the
        // held frame must not block in a one-frame queue.
        p.seek(1.21);
        let mut between = None;
        within("the frame at 1.21s", 3000, Box::new(|| {
            between = between.take().or_else(|| p.take_frame());
            between.is_some()
        }));
        let f = between.unwrap();
        assert!((f.pts - 1.2).abs() < 0.005, "showed {:.3}s for a seek to 1.21s", f.pts);

        // Back on screen it plays on as normal.
        p.set_background(false);
        p.set_paused(false);
        within("playback past 1.5s", 3000, Box::new(|| p.take_frame().is_some_and(|f| f.pts > 1.5)));
    }

    #[test]
    fn probe_reads_tags_and_duration_without_decoding() {
        let s = probe(&fixture("tagged.mp3")).unwrap();
        assert_eq!((s.title.as_deref(), s.artist.as_deref(), s.has_video), (Some("Test Tone"), Some("Fixture Band"), false));
        assert!((s.duration.unwrap() - 2.0).abs() < 0.1, "{:?}", s.duration);
        let s = probe(&fixture("av1_opus.webm")).unwrap();
        assert!(s.has_video && s.title.is_none());
        assert!((s.duration.unwrap() - 3.0).abs() < 0.1, "{:?}", s.duration);
        assert!(probe(&fixture("generate.sh")).is_err());
    }

    #[test]
    fn tags_are_read() {
        let path = fixture("tagged.mp3");
        let mss = MediaSourceStream::new(Box::new(File::open(&path).unwrap()), Default::default());
        let mut format = symphonia::default::get_probe()
            .probe(Hint::new().with_extension("mp3"), mss, FormatOptions::default(), MetadataOptions::default())
            .unwrap();
        let mut info = MediaInfo::default();
        read_metadata(format.as_mut(), &mut info);
        assert_eq!(info.title, "Test Tone");
        assert_eq!(info.artist.as_deref(), Some("Fixture Band"));
        assert_eq!(info.album.as_deref(), Some("Fixtures"));
    }
}
