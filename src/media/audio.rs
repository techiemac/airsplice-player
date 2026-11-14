//! Audio output, the playback clock, and conversion of decoded audio into the
//! device's channel layout and sample rate.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context as _, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, Stream, StreamConfig};
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
}

pub struct AudioOutput {
    _stream: Stream,
    pub state: Arc<Mutex<AudioState>>,
    pub rate: u32,
    pub channels: usize,
}

impl AudioOutput {
    pub fn open(volume: f32, muted: bool) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no audio output device"))?;
        let supported = device
            .default_output_config()
            .context("querying the default audio output config")?;
        let format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let rate = config.sample_rate;
        let channels = config.channels as usize;

        let state = Arc::new(Mutex::new(AudioState {
            buf: VecDeque::with_capacity(rate as usize * channels),
            consumed_frames: 0,
            base_secs: 0.0,
            generation: 0,
            paused: false,
            primed: true,
            volume,
            muted,
            peak: [0.0; 2],
        }));

        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, &config, state.clone()),
            SampleFormat::I16 => build::<i16>(&device, &config, state.clone()),
            SampleFormat::I32 => build::<i32>(&device, &config, state.clone()),
            SampleFormat::U16 => build::<u16>(&device, &config, state.clone()),
            SampleFormat::F64 => build::<f64>(&device, &config, state.clone()),
            other => Err(anyhow!("unsupported audio device sample format {other:?}")),
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
    let stream = device.build_output_stream(
        *config,
        move |out: &mut [T], _| {
            let mut st = state.lock().unwrap();
            let gain = if st.muted { 0.0 } else { st.volume * st.volume };
            let silence = T::from_sample(0.0f32);
            if st.paused || !st.primed {
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
            st.consumed_frames += (available / channels) as u64;
            // Decay so the meter falls smoothly instead of snapping to zero.
            for (held, new) in st.peak.iter_mut().zip(peak) {
                *held = new.max(*held * 0.85);
            }
        },
        |err| log::error!("audio stream error: {err}"),
        None,
    )?;
    Ok(stream)
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
            Clock::Audio { state, rate, .. } => {
                let st = state.lock().unwrap();
                st.base_secs + st.consumed_frames as f64 / *rate as f64
            }
            Clock::Wall(w) => {
                let w = w.lock().unwrap();
                w.base_secs + w.started.map_or(0.0, |s| s.elapsed().as_secs_f64())
            }
        }
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
                st.buf.clear();
                st.consumed_frames = 0;
                st.base_secs = secs;
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
