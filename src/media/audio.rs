//! Audio output, the playback clock, and conversion of decoded audio into the
//! device's channel layout and sample rate.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context as _, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, FromSample, SampleFormat, SizedSample, Stream, StreamConfig, SupportedBufferSize};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

/// Shared between the decode thread (producer), the device callback
/// (consumer) and the UI (clock reader, seek, volume).
pub struct AudioState {
    /// Interleaved f32 samples in the device layout, waiting to be played.
    pub buf: VecDeque<f32>,
    /// Frames handed to the device since the last seek.
    pub consumed_frames: u64,
    /// Media time, in seconds, that `consumed_frames == 0` corresponds to.
    pub base_secs: f64,
    /// Seek generation the buffered samples belong to.
    pub generation: u64,
    pub paused: bool,
    /// False while waiting for the first video frame (pre-roll): the device
    /// plays silence and the clock holds, so video does not start late.
    pub primed: bool,
    pub volume: f32,
    pub muted: bool,
    /// Most recent peak level per channel, for the level meter.
    pub peak: [f32; 2],
    /// Frames handed to the device but not yet heard (its buffer latency),
    /// from the callback's timestamps. The clock subtracts it.
    pub latency_frames: u64,
    /// The stream's audio has all been decoded (end of file). Once the
    /// buffer is empty the clock keeps running on the silence played, so
    /// video that outlasts the audio still plays to its end.
    pub ended: bool,
    /// Times the device ran dry mid-stream (the decoder fell behind), a
    /// gap the listener hears. Read by the UI's performance warning.
    pub dropouts: u64,
    /// The last callback was filled, so running dry now is a dropout (not
    /// the wait for the first audio after open or a seek).
    flowing: bool,
    /// When the last callback ran and how many frames it played: between
    /// callbacks the clock moves on with the wall clock, by at most that
    /// many frames.
    last_fill: Option<Instant>,
    last_played: u64,
    /// The latest time the clock reported, so it never steps backwards.
    last_now: f64,
}

impl AudioState {
    fn new(volume: f32, muted: bool, buf: VecDeque<f32>) -> Self {
        AudioState {
            buf,
            consumed_frames: 0,
            base_secs: 0.0,
            generation: 0,
            paused: false,
            primed: true,
            volume,
            muted,
            peak: [0.0; 2],
            latency_frames: 0,
            ended: false,
            dropouts: 0,
            flowing: false,
            last_fill: None,
            last_played: 0,
            last_now: f64::NEG_INFINITY,
        }
    }

    /// Media time being heard at `at`. Devices take audio in blocks (20 ms
    /// here, often more), and a clock that only moved when one was taken
    /// would jump a block at a time: at 60 fps two frames would come due at
    /// once and one never be shown. So between blocks it runs on the wall
    /// clock, never past the frames the last block played, and never
    /// backwards (the latency estimate jitters).
    fn now_at(&mut self, rate: u32, at: Instant) -> f64 {
        let rate = rate as f64;
        let mut secs = self.base_secs + self.consumed_frames.saturating_sub(self.latency_frames) as f64 / rate;
        let heard = self.consumed_frames > self.latency_frames;
        if let Some(last) = self.last_fill.filter(|_| heard && self.primed && !self.paused) {
            secs += at.saturating_duration_since(last).as_secs_f64().min(self.last_played as f64 / rate);
        }
        self.last_now = self.last_now.max(secs);
        self.last_now
    }
}

pub struct AudioOutput {
    _stream: Stream,
    pub state: Arc<Mutex<AudioState>>,
    pub rate: u32,
    pub channels: usize,
}

/// An audio output device the user can choose.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputDevice {
    /// Stable across reconnects and restarts; what the setting stores.
    pub id: String,
    pub name: String,
}

/// The output devices connected now, in the order the system lists them.
pub fn output_devices() -> Vec<OutputDevice> {
    let host = cpal::default_host();
    let devices = match host.output_devices() {
        Ok(devices) => devices,
        Err(e) => {
            log::warn!("listing audio outputs: {e}");
            return Vec::new();
        }
    };
    let mut list: Vec<OutputDevice> = Vec::new();
    for device in devices {
        let Ok(id) = device.id() else { continue };
        #[cfg(target_os = "linux")]
        let alsa = id.host() == cpal::HostId::Alsa;
        #[cfg(not(target_os = "linux"))]
        let alsa = false;
        #[cfg(target_os = "linux")]
        if alsa && !is_alsa_output(id.id()) {
            continue;
        }
        let name = device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| id.id().to_string());
        let id = id.to_string();
        // ALSA lists each card by number and by name: the same output twice.
        // Elsewhere two outputs may share a name (two HDMI ports).
        if !list.iter().any(|d| d.id == id || (alsa && d.name == name)) {
            list.push(OutputDevice { id, name });
        }
    }
    list
}

/// Without a sound server, ALSA lists every alias of every output: default,
/// sysdefault, hdmi, hw, plughw, dmix and more, per card. Only `plughw`
/// (the hardware with format conversion) is kept: one entry per output that
/// opens with any stream format. The system default is offered separately.
#[cfg(any(target_os = "linux", test))]
fn is_alsa_output(pcm_id: &str) -> bool {
    pcm_id.starts_with("plughw:")
}

/// Set to act as if no audio output is connected (diagnostics, and its
/// tests).
pub const NO_OUTPUT_ENV: &str = "AIRSPLICE_PLAYER_NO_AUDIO_OUTPUT";

/// The device with stable id `id`, if it is connected; else the system's
/// default output.
fn output_device(host: &cpal::Host, id: Option<&str>) -> Result<cpal::Device> {
    if std::env::var_os(NO_OUTPUT_ENV).is_some() {
        bail!("no audio output device");
    }
    if let Some(id) = id {
        match id.parse::<cpal::DeviceId>().ok().and_then(|parsed| host.device_by_id(&parsed)) {
            Some(device) => return Ok(device),
            None => log::info!("audio output {id} is not connected; using the system default"),
        }
    }
    host.default_output_device().ok_or_else(|| anyhow!("no audio output device"))
}

impl AudioOutput {
    /// Open the output device with stable id `device` (see
    /// [`output_devices`]), or the system default when that is `None` or
    /// not connected.
    pub fn open(device: Option<&str>, volume: f32, muted: bool) -> Result<Self> {
        let host = cpal::default_host();
        let device = output_device(&host, device)?;
        let supported = device
            .default_output_config()
            .context("querying the default audio output config")?;
        let format = supported.sample_format();
        let buffer_range = *supported.buffer_size();
        let mut config: StreamConfig = supported.into();
        let rate = config.sample_rate;
        let channels = config.channels as usize;
        // Ask for ~20 ms device buffers. Some backends default to seconds
        // (ALSA through PulseAudio: ~2 s), which delays the start and, if
        // uncompensated, desyncs video.
        let small = match buffer_range {
            SupportedBufferSize::Range { min, max } => Some(BufferSize::Fixed((rate / 50).clamp(min, max))),
            SupportedBufferSize::Unknown => None,
        };

        let state = Arc::new(Mutex::new(AudioState::new(volume, muted, VecDeque::with_capacity(rate as usize * channels))));

        let build_with = |config: &StreamConfig| match format {
            SampleFormat::F32 => build::<f32>(&device, config, state.clone()),
            SampleFormat::I16 => build::<i16>(&device, config, state.clone()),
            SampleFormat::I32 => build::<i32>(&device, config, state.clone()),
            SampleFormat::U16 => build::<u16>(&device, config, state.clone()),
            SampleFormat::F64 => build::<f64>(&device, config, state.clone()),
            other => Err(anyhow!("unsupported audio device sample format {other:?}")),
        };
        let stream = match small {
            Some(size) => {
                let default_size = std::mem::replace(&mut config.buffer_size, size);
                build_with(&config).or_else(|e| {
                    log::info!("audio device refused a small buffer ({e:#}); using its default");
                    config.buffer_size = default_size;
                    build_with(&config)
                })
            }
            None => build_with(&config),
        }?;
        stream.play().context("starting audio output")?;

        Ok(Self { _stream: stream, state, rate, channels })
    }

    /// Seconds of audio the producer may keep queued ahead of the device.
    pub fn capacity_samples(&self) -> usize {
        (self.rate as usize * self.channels) / 2
    }
}

fn build<T>(device: &cpal::Device, config: &StreamConfig, state: Arc<Mutex<AudioState>>) -> Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let rate = config.sample_rate as f64;
    let stream = device.build_output_stream(
        *config,
        move |out: &mut [T], info: &cpal::OutputCallbackInfo| {
            let ts = info.timestamp();
            let latency = ts.playback.duration_since(ts.callback).as_secs_f64();
            let mut st = state.lock().unwrap();
            st.latency_frames = (latency * rate) as u64;
            fill(&mut st, out, channels);
        },
        |err| log::error!("audio stream error: {err}"),
        None,
    )?;
    Ok(stream)
}

/// One device callback's worth of samples: drains the queue into `out`,
/// fills the rest with silence, advances the clock and counts a dropout
/// when the queue runs dry mid-stream.
fn fill<T>(st: &mut AudioState, out: &mut [T], channels: usize)
where
    T: SizedSample + FromSample<f32>,
{
    let gain = if st.muted { 0.0 } else { st.volume * st.volume };
    let silence = T::from_sample(0.0f32);
    st.last_fill = Some(Instant::now());
    if st.paused || !st.primed {
        st.flowing &= st.primed;
        st.last_played = 0;
        out.fill(silence);
        st.peak = [0.0; 2];
        return;
    }
    let available = st.buf.len().min(out.len());
    let available = available - available % channels;
    let mut peak = [0.0f32; 2];
    for (i, slot) in out[..available].iter_mut().enumerate() {
        let s = st.buf.pop_front().unwrap_or(0.0) * gain;
        let p = &mut peak[(i % channels).min(1)];
        *p = p.max(s.abs());
        *slot = T::from_sample(s);
    }
    out[available..].fill(silence);
    if !st.ended {
        let full = available == out.len();
        if !full && st.flowing {
            st.dropouts += 1;
        }
        st.flowing = full;
    }
    // Running dry mid-stream (a slow decoder) holds the clock so the
    // picture waits for the sound; after the end, silence counts.
    let played = if st.ended { out.len() } else { available };
    st.consumed_frames += (played / channels) as u64;
    st.last_played = (played / channels) as u64;
    // Decay so the meter falls smoothly instead of snapping to zero.
    for (held, new) in st.peak.iter_mut().zip(peak) {
        *held = new.max(*held * 0.85);
    }
}

/// The media clock. Audio-driven when the file has audio, wall-clock driven
/// otherwise.
pub enum Clock {
    /// `preroll`: the media has video, so playback holds after open and
    /// after each seek until the first frame is ready.
    Audio { state: Arc<Mutex<AudioState>>, rate: u32, preroll: bool },
    Wall(Mutex<WallClock>),
}

/// Video-only playback. The clock is armed on open and on every seek but
/// only starts running once the first frame is ready (`prime`), so the
/// first frames are not late by however long decoding took to start.
pub struct WallClock {
    base_secs: f64,
    started: Option<Instant>,
    paused: bool,
    primed: bool,
}

impl Clock {
    pub fn wall() -> Self {
        Clock::Wall(Mutex::new(WallClock { base_secs: 0.0, started: None, paused: false, primed: false }))
    }

    pub fn now(&self) -> f64 {
        match self {
            // Frames still in the device buffer have not been heard yet.
            Clock::Audio { state, rate, .. } => state.lock().unwrap().now_at(*rate, Instant::now()),
            Clock::Wall(w) => {
                let w = w.lock().unwrap();
                w.base_secs + w.started.map_or(0.0, |s| s.elapsed().as_secs_f64())
            }
        }
    }

    /// The time, while the clock is running: primed and not paused. Video
    /// can only be late against a running clock.
    pub fn running(&self) -> Option<f64> {
        let running = match self {
            Clock::Audio { state, .. } => {
                let st = state.lock().unwrap();
                st.primed && !st.paused
            }
            Clock::Wall(w) => {
                let w = w.lock().unwrap();
                w.primed && !w.paused
            }
        };
        running.then(|| self.now())
    }

    pub fn set_paused(&self, paused: bool) {
        match self {
            Clock::Audio { state, .. } => state.lock().unwrap().paused = paused,
            Clock::Wall(w) => {
                let mut w = w.lock().unwrap();
                w.paused = paused;
                if let Some(s) = w.started.take() {
                    w.base_secs += s.elapsed().as_secs_f64();
                }
                if !paused && w.primed {
                    w.started = Some(Instant::now());
                }
            }
        }
    }

    /// Jump to `secs`, discarding queued audio. `generation` tags which
    /// decoded samples are still valid.
    pub fn seek(&self, secs: f64, generation: u64) {
        match self {
            Clock::Audio { state, preroll, .. } => {
                let mut st = state.lock().unwrap();
                st.primed = !*preroll;
                st.ended = false;
                st.buf.clear();
                st.flowing = false;
                st.consumed_frames = 0;
                st.base_secs = secs;
                (st.last_fill, st.last_played, st.last_now) = (None, 0, f64::NEG_INFINITY);
                st.generation = generation;
            }
            Clock::Wall(w) => {
                let mut w = w.lock().unwrap();
                w.base_secs = secs;
                w.started = None;
                w.primed = false;
            }
        }
    }

    /// Start a wall clock that is waiting for its first frame.
    pub fn is_primed(&self) -> bool {
        match self {
            Clock::Audio { state, .. } => state.lock().unwrap().primed,
            Clock::Wall(w) => w.lock().unwrap().primed,
        }
    }

    /// Let a clock held for pre-roll run.
    pub fn prime(&self) {
        match self {
            Clock::Audio { state, .. } => state.lock().unwrap().primed = true,
            Clock::Wall(w) => {
                let mut w = w.lock().unwrap();
                if !w.primed {
                    w.primed = true;
                    if !w.paused {
                        w.started = Some(Instant::now());
                    }
                }
            }
        }
    }
}

/// Converts decoded audio (any channel count, any rate) to the device layout
/// and rate.
pub struct Converter {
    src_channels: usize,
    dst_channels: usize,
    resampler: Option<Fft<f32>>,
    /// Interleaved device-layout samples waiting for a full resampler chunk.
    pending: Vec<f32>,
    /// Output frames still to drop to hide the resampler's startup delay.
    skip_frames: usize,
    mixed: Vec<f32>,
}

const CHUNK: usize = 1024;

impl Converter {
    pub fn new(src_rate: u32, src_channels: usize, dst_rate: u32, dst_channels: usize) -> Result<Self> {
        let resampler = if src_rate != dst_rate {
            Some(
                Fft::<f32>::new(src_rate as usize, dst_rate as usize, CHUNK, dst_channels, FixedSync::Input)
                    .map_err(|e| anyhow!("creating resampler {src_rate} Hz -> {dst_rate} Hz: {e}"))?,
            )
        } else {
            None
        };
        let skip_frames = resampler.as_ref().map_or(0, |r| r.output_delay());
        Ok(Self { src_channels, dst_channels, resampler, pending: Vec::new(), skip_frames, mixed: Vec::new() })
    }

    pub fn reset(&mut self) {
        self.pending.clear();
        if let Some(r) = self.resampler.as_mut() {
            r.reset();
            self.skip_frames = r.output_delay();
        }
    }

    /// Feed interleaved source samples; appends device-ready samples to `out`.
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.mixed.clear();
        remix(input, self.src_channels, self.dst_channels, &mut self.mixed);
        let Some(resampler) = self.resampler.as_mut() else {
            out.extend_from_slice(&self.mixed);
            return;
        };
        self.pending.extend_from_slice(&self.mixed);
        let ch = self.dst_channels;
        let mut offset = 0;
        while self.pending.len() - offset >= CHUNK * ch {
            let chunk = &self.pending[offset..offset + CHUNK * ch];
            offset += CHUNK * ch;
            let adapter = InterleavedSlice::new(chunk, ch, CHUNK).expect("chunk sized to the resampler");
            match resampler.process(&adapter, None) {
                Ok(buf) => {
                    let data = buf.take_data();
                    let skip = (self.skip_frames * ch).min(data.len());
                    self.skip_frames -= skip / ch;
                    out.extend_from_slice(&data[skip..]);
                }
                Err(e) => log::error!("resampler: {e}"),
            }
        }
        self.pending.drain(..offset);
    }
}

/// Map `src` channels onto `dst` channels. Symphonia orders channels by
/// position bit: FL, FR, FC, LFE, RL, RR, ...
fn remix(input: &[f32], src: usize, dst: usize, out: &mut Vec<f32>) {
    let frames = input.len() / src.max(1);
    out.reserve(frames * dst);
    if src == dst {
        out.extend_from_slice(&input[..frames * src]);
        return;
    }
    const C: f32 = std::f32::consts::FRAC_1_SQRT_2;
    for f in input.chunks_exact(src) {
        match (src, dst) {
            (1, _) => out.extend(std::iter::repeat_n(f[0], dst)),
            (_, 1) => out.push(f.iter().sum::<f32>() / src as f32),
            (s, 2) if s >= 6 => {
                // ITU-R BS.775 downmix, scaled to stay within [-1, 1].
                let norm = 1.0 / (1.0 + 2.0 * C);
                out.push((f[0] + C * f[2] + C * f[4]) * norm);
                out.push((f[1] + C * f[2] + C * f[5]) * norm);
            }
            _ => out.extend((0..dst).map(|c| f.get(c).copied().unwrap_or(0.0))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_is_duplicated_to_stereo() {
        let mut out = Vec::new();
        remix(&[0.5, -0.25], 1, 2, &mut out);
        assert_eq!(out, vec![0.5, 0.5, -0.25, -0.25]);
    }

    #[test]
    fn surround_downmix_stays_in_range() {
        let mut out = Vec::new();
        remix(&[1.0; 6], 6, 2, &mut out);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|s| (*s - 1.0).abs() < 1e-6), "{out:?}");
    }

    #[test]
    fn resampler_converts_rate_and_drops_startup_delay() {
        let mut conv = Converter::new(44_100, 2, 48_000, 2).unwrap();
        let input = vec![0.1f32; 44_100 * 2];
        let mut out = Vec::new();
        conv.push(&input, &mut out);
        let frames = out.len() / 2;
        // One second in, slightly less than one second out (delay and the
        // trailing partial chunk are held back).
        assert!(frames > 46_000 && frames <= 48_000, "{frames}");
        // After the startup delay a DC input stays DC.
        assert!(out[out.len() / 2..].iter().all(|s| (s - 0.1).abs() < 0.01));
    }
}

#[cfg(test)]
mod alsa_list_tests {
    use super::is_alsa_output;

    #[test]
    fn alsa_lists_each_output_once() {
        // What ALSA lists on a Raspberry Pi 5 with two HDMI ports.
        let listed = [
            "default", "pipewire", "pulse", "sysdefault:CARD=vc4hdmi0", "hdmi:CARD=vc4hdmi0,DEV=0", "hw:CARD=vc4hdmi0,DEV=0",
            "plughw:CARD=vc4hdmi0,DEV=0", "dmix:CARD=vc4hdmi0,DEV=0", "plughw:CARD=vc4hdmi1,DEV=0", "hw:CARD=1,DEV=0",
        ];
        let kept: Vec<_> = listed.into_iter().filter(|id| is_alsa_output(id)).collect();
        assert_eq!(kept, ["plughw:CARD=vc4hdmi0,DEV=0", "plughw:CARD=vc4hdmi1,DEV=0"]);
    }
}

#[cfg(test)]
mod dropout_tests {
    use super::*;

    fn state(queued: usize) -> AudioState {
        AudioState::new(1.0, false, std::iter::repeat_n(0.1, queued).collect())
    }

    /// Between device blocks the clock runs on the wall clock, by no more
    /// than the last block played, and never backwards.
    #[test]
    fn the_clock_moves_smoothly_between_device_blocks() {
        let ms = |n: u64| std::time::Duration::from_millis(n);
        let mut st = state(0);
        let t0 = Instant::now();
        // A 20 ms block (960 frames at 48 kHz) heard from 1 s in.
        (st.consumed_frames, st.last_played, st.last_fill) = (48_000, 960, Some(t0));
        assert_eq!(st.now_at(48_000, t0), 1.0);
        assert!((st.now_at(48_000, t0 + ms(8)) - 1.008).abs() < 1e-9, "moves between blocks");
        assert!((st.now_at(48_000, t0 + ms(50)) - 1.020).abs() < 1e-9, "never past the block");
        // The next block lands a little early: the clock holds, not steps back.
        (st.consumed_frames, st.last_fill) = (48_960, Some(t0 + ms(19)));
        assert!((st.now_at(48_000, t0 + ms(19)) - 1.020).abs() < 1e-9);
        assert!((st.now_at(48_000, t0 + ms(25)) - 1.026).abs() < 1e-9);
        // Latency jitter that would move it back is held.
        st.latency_frames = 480;
        assert!((st.now_at(48_000, t0 + ms(25)) - 1.026).abs() < 1e-9);
    }

    #[test]
    fn the_clock_holds_while_paused_dry_or_not_yet_heard() {
        let ms = |n: u64| std::time::Duration::from_millis(n);
        let t0 = Instant::now();
        let mut st = state(0);
        (st.consumed_frames, st.last_played, st.last_fill, st.paused) = (48_000, 960, Some(t0), true);
        assert_eq!(st.now_at(48_000, t0 + ms(10)), 1.0, "paused");
        let mut st = state(0);
        (st.consumed_frames, st.last_played, st.last_fill, st.primed) = (48_000, 960, Some(t0), false);
        assert_eq!(st.now_at(48_000, t0 + ms(10)), 1.0, "waiting for the first frame");
        let mut st = state(0);
        (st.consumed_frames, st.latency_frames, st.last_played, st.last_fill) = (960, 1920, 960, Some(t0));
        assert_eq!(st.now_at(48_000, t0 + ms(10)), 0.0, "still in the device buffer");
        // A block that ran dry: only what it played counts.
        let mut st = state(4);
        let mut out = [0.0f32; 8];
        fill(&mut st, &mut out, 2);
        assert_eq!(st.last_played, 2);
        let at = st.last_fill.unwrap();
        assert!((st.now_at(1000, at + ms(100)) - 0.004).abs() < 1e-9, "2 frames + 2 frames at 1 kHz");
    }

    #[test]
    fn running_dry_mid_stream_is_a_dropout() {
        let mut st = state(6);
        let mut out = [0.0f32; 4];
        fill(&mut st, &mut out, 2);
        assert_eq!(st.dropouts, 0);
        // Two samples left for four slots: a gap the listener hears.
        fill(&mut st, &mut out, 2);
        assert_eq!(st.dropouts, 1);
        // Still dry: the same gap, not a new one.
        fill(&mut st, &mut out, 2);
        assert_eq!(st.dropouts, 1);
        st.buf.extend([0.1; 8]);
        fill(&mut st, &mut out, 2);
        fill(&mut st, &mut out, 2);
        fill(&mut st, &mut out, 2);
        assert_eq!(st.dropouts, 2);
    }

    #[test]
    fn waiting_for_the_first_audio_and_the_end_are_not_dropouts() {
        let mut out = [0.0f32; 4];
        // Nothing queued yet after open or a seek.
        let mut st = state(0);
        fill(&mut st, &mut out, 2);
        assert_eq!(st.dropouts, 0);
        // A seek empties a flowing queue and pre-rolls.
        let mut st = state(4);
        fill(&mut st, &mut out, 2);
        st.primed = false;
        st.buf.clear();
        fill(&mut st, &mut out, 2);
        st.primed = true;
        fill(&mut st, &mut out, 2);
        assert_eq!(st.dropouts, 0);
        // The end of the file.
        let mut st = state(4);
        fill(&mut st, &mut out, 2);
        st.ended = true;
        fill(&mut st, &mut out, 2);
        assert_eq!(st.dropouts, 0);
    }

    #[test]
    fn a_pause_is_not_a_dropout() {
        let mut st = state(8);
        let mut out = [0.0f32; 4];
        fill(&mut st, &mut out, 2);
        st.paused = true;
        fill(&mut st, &mut out, 2);
        st.paused = false;
        fill(&mut st, &mut out, 2);
        assert_eq!(st.dropouts, 0);
    }
}
