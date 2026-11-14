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

use std::collections::VecDeque;
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
use super::video::{self, DecodedFrame, VideoDecoder};

/// Decoded frames kept ahead of the clock.
const MAX_QUEUED_FRAMES: usize = 8;
/// Video packets buffered between the demuxer and the video decoder.
const VIDEO_PACKET_QUEUE: usize = 96;
const NO_GENERATION: u64 = u64::MAX;
/// Pre-roll frames this far before a seek target count as "at" the target.
const PREROLL_TOLERANCE: f64 = 0.1;

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

/// Seek so the first video packet read is a keyframe, backing off 1 s, 2 s,
/// 4 s... before the target when it is not. Containers differ in how (and
/// whether) they honour sync samples, so this does not rely on them.
/// Returns packets already read, to be processed before reading more.
fn seek_to_keyframe(
    format: &mut Box<dyn FormatReader>,
    path: &Path,
    exhausted: &mut bool,
    target: f64,
    seek_track: Option<u32>,
    video: Option<&VideoRoute>,
) -> VecDeque<symphonia::core::packet::Packet> {
    let mut back = 0.0;
    loop {
        let start = (target - back).max(0.0);
        seek_reader(format, path, exhausted, start, seek_track);
        let Some(v) = video else { return VecDeque::new() };
        let mut pending = VecDeque::new();
        let first_video_is_key = loop {
            match format.next_packet() {
                Ok(Some(p)) => {
                    let key = (p.track_id == v.track_id).then(|| (v.is_keyframe)(&p.data));
                    pending.push_back(p);
                    if key.is_some() || pending.len() > 4096 {
                        break key;
                    }
                }
                // End of stream or an error: let the main loop handle it,
                // and reopen before any further seek.
                _ => {
                    *exhausted = true;
                    break None;
                }
            }
        };
        match first_video_is_key {
            Some(false) if start > 0.0 => back = if back == 0.0 { 1.0 } else { back * 2.0 },
            _ => return pending,
        }
    }
}

pub struct Player {
    pub info: MediaInfo,
    shared: Arc<Shared>,
    clock: Arc<Clock>,
    audio: Option<AudioOutput>,
    paused: bool,
}

impl Player {
    pub fn open(path: &Path, volume: f32, muted: bool) -> Result<Self> {
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

        let shared = Arc::new(Shared {
            generation: AtomicU64::new(0),
            seek_target: Mutex::new(0.0),
            frames: Mutex::new(VecDeque::new()),
            frames_cv: Condvar::new(),
            demux_done: AtomicU64::new(NO_GENERATION),
            video_done: AtomicU64::new(NO_GENERATION),
            error: Mutex::new(None),
            stop: AtomicBool::new(false),
        });

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
            Some(out) => Clock::Audio { state: out.state.clone(), rate: out.rate },
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
        {
            let shared = shared.clone();
            let path = path.to_path_buf();
            thread::Builder::new()
                .name("demux".into())
                .spawn(move || demux_thread(format, path, audio_pipeline, video_route, seek_track, shared))?;
        }

        Ok(Self { info, shared, clock, audio: audio_out, paused: false })
    }

    pub fn position(&self) -> f64 {
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

    pub fn levels(&self) -> [f32; 2] {
        self.audio.as_ref().map_or([0.0; 2], |o| o.state.lock().unwrap().peak)
    }

    /// The newest decoded frame whose time has come, dropping any older ones.
    pub fn take_frame(&self) -> Option<VideoFrame> {
        let generation = self.shared.current();
        if self.shared.frames.lock().unwrap().iter().any(|f| f.generation == generation) {
            self.clock.prime();
        }
        let now = self.clock.now();
        let target = *self.shared.seek_target.lock().unwrap();
        let mut frames = self.shared.frames.lock().unwrap();
        let mut best = None;
        while let Some(front) = frames.front() {
            if front.generation != generation {
                frames.pop_front();
            } else if front.pts <= now + 0.004 {
                best = frames.pop_front();
            } else {
                break;
            }
        }
        // Right after a seek the decoder works through pre-roll frames from
        // the previous keyframe. Show one only if it is provably the frame
        // on screen at the target (its successor is past the target);
        // otherwise hold it until that is known.
        if let Some(f) = best.take_if(|f| {
            f.pts < target - PREROLL_TOLERANCE && !frames.front().is_some_and(|next| next.pts > target)
        }) {
            frames.push_front(f);
        }
        drop(frames);
        self.shared.frames_cv.notify_all();
        best
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    pub fn is_finished(&self) -> bool {
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
    for px in bgra.chunks_exact_mut(4) {
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
            let pkt_start = self.time_base.calc_time(packet.pts).map_or(0.0, |t| t.as_secs_f64());
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
    shared: Arc<Shared>,
) {
    let mut generation = 0;
    let mut eof = false;
    let mut skip_until: Option<f64> = None;
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
            pending = seek_to_keyframe(&mut format, &path, &mut exhausted, target, seek_track, video.as_ref());
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
            None => format.next_packet(),
        };
        let packet = match next {
            Ok(Some(p)) => p,
            Ok(None) => {
                eof = true;
                exhausted = true;
                if let Some(v) = &video {
                    let _ = v.tx.send(VideoMsg::EndOfStream { generation });
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

fn video_thread(mut decoder: Box<dyn VideoDecoder>, rx: Receiver<VideoMsg>, time_base: TimeBase, shared: Arc<Shared>) {
    let push = |frames: Vec<DecodedFrame>, generation: u64| {
        for f in frames {
            let pts = time_base.calc_time(Timestamp::new(f.pts)).map_or(0.0, |t| t.as_secs_f64());
            let mut q = shared.frames.lock().unwrap();
            while q.len() >= MAX_QUEUED_FRAMES && shared.current() == generation && !shared.stopped() {
                q = shared.frames_cv.wait_timeout(q, StdDuration::from_millis(100)).unwrap().0;
            }
            if shared.current() != generation || shared.stopped() {
                return;
            }
            // Decoders may emit in decode order; keep the queue in PTS order.
            let at = q.iter().position(|e| e.pts > pts).unwrap_or(q.len());
            q.insert(at, VideoFrame { pts, generation, width: f.width, height: f.height, bgra: f.bgra });
        }
    };
    while let Ok(msg) = rx.recv() {
        if shared.stopped() {
            return;
        }
        match msg {
            VideoMsg::Packet { generation, pts, data } => {
                if generation != shared.current() {
                    continue;
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
