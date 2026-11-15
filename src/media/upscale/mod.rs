//! Real-time upscaling of video frames to the size they are shown at.
//!
//! Frames are scaled up on the GPU in the video thread, before they are
//! queued, by the best upscaler the hardware offers: MetalFX on Apple GPUs
//! (the system's own real-time upscaler) and AMD FSR 1 on any other GPU
//! through wgpu (Vulkan, Direct3D 12 or Metal). Both are spatial: they
//! reconstruct edges from the frame itself and invent no detail, so the
//! picture stays faithful to the source. Without a hardware GPU there is no
//! upscaling, and the GPU's plain bilinear scaling at draw time remains.

mod fsr;
#[cfg(target_os = "macos")]
mod metalfx;

use std::sync::OnceLock;
use std::time::Instant;

pub use fsr::Fsr;

/// Upscaling beyond this per axis is left to the display's bilinear
/// scaling: spatial upscalers reconstruct edges well up to about 2x and
/// only add cost beyond it.
pub const MAX_FACTOR: f64 = 2.0;
/// Below this gain the result is indistinguishable from bilinear scaling,
/// so the work is skipped.
pub const MIN_GAIN: f64 = 1.1;
/// Consecutive frames over budget before upscaling stops for the file.
const SLOW_FRAMES: u32 = 10;
/// The share of a frame's display time that upscaling may take; decoding
/// needs the rest.
const BUDGET_SHARE: f64 = 0.5;

pub trait Upscaler {
    /// Scale a BGRA frame to `out_width` x `out_height`, returning BGRA.
    fn upscale(&mut self, bgra: &[u8], width: u32, height: u32, out_width: u32, out_height: u32) -> anyhow::Result<Vec<u8>>;
}

/// What upscaling is doing for the file on screen.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// Switched off in the settings.
    Off,
    /// The video already fills the display at (close to) its own size.
    NotNeeded,
    Active { from: (u32, u32), to: (u32, u32) },
    /// Stopped for this file: it could not keep up with the frame rate.
    TooSlow,
    /// Stopped for this file: the GPU reported an error.
    Failed(String),
}

/// The upscaler this machine would use, by name, or `None` without a
/// suitable GPU. Probed once.
pub fn available() -> Option<&'static str> {
    static NAME: OnceLock<Option<String>> = OnceLock::new();
    NAME.get_or_init(|| best().map(|(name, _)| name)).as_deref()
}

/// Probe in the background, so the settings can name the upscaler
/// without waiting on GPU initialisation.
pub fn warm_up() {
    let _ = std::thread::Builder::new().name("upscale-probe".into()).spawn(|| {
        available();
    });
}

/// The best upscaler for this machine, with its name.
fn best() -> Option<(String, Box<dyn Upscaler>)> {
    #[cfg(target_os = "macos")]
    if let Some(m) = metalfx::MetalFx::new() {
        return Some((m.name(), Box::new(m)));
    }
    let fsr = Fsr::new(false)?;
    Some((fsr.name(), Box::new(fsr)))
}

/// Every upscaler this machine offers, best first, with names: for
/// benchmarks and comparisons.
pub fn all() -> Vec<(String, Box<dyn Upscaler>)> {
    let mut all: Vec<(String, Box<dyn Upscaler>)> = Vec::new();
    #[cfg(target_os = "macos")]
    if let Some(m) = metalfx::MetalFx::new() {
        all.push((m.name(), Box::new(m)));
    }
    if let Some(f) = Fsr::new(false) {
        all.push((f.name(), Box::new(f)));
    }
    all
}

/// The size to upscale a `src` frame to so it fills (but does not exceed)
/// `limit`, keeping its shape; `None` when that would gain too little.
pub fn target_size(src: (u32, u32), limit: (u32, u32)) -> Option<(u32, u32)> {
    if src.0 == 0 || src.1 == 0 || limit.0 == 0 || limit.1 == 0 {
        return None;
    }
    let fit = (limit.0 as f64 / src.0 as f64).min(limit.1 as f64 / src.1 as f64);
    let factor = fit.min(MAX_FACTOR);
    if factor < MIN_GAIN {
        return None;
    }
    Some(((src.0 as f64 * factor).round() as u32, (src.1 as f64 * factor).round() as u32))
}

type Factory = Box<dyn FnMut() -> Option<Box<dyn Upscaler>> + Send>;
type Clock = Box<dyn FnMut() -> Instant + Send>;

/// Upscaling in one video thread: creates the upscaler when first needed,
/// and stops for the file if it errors or cannot keep up.
pub struct Stage {
    factory: Factory,
    /// Times each upscale; a fake one in tests.
    clock: Clock,
    upscaler: Option<Box<dyn Upscaler>>,
    tried: bool,
    stopped: Option<Status>,
    slow: u32,
    /// Smoothed time between frames, from their timestamps.
    interval: Option<f64>,
    last_pts: Option<f64>,
}

impl Default for Stage {
    fn default() -> Self {
        Self::with_factory(Box::new(|| best().map(|(_, u)| u)))
    }
}

impl Stage {
    pub fn with_factory(factory: Factory) -> Self {
        Self::with_factory_and_clock(factory, Box::new(Instant::now))
    }

    pub fn with_factory_and_clock(factory: Factory, clock: Clock) -> Self {
        Self { factory, clock, upscaler: None, tried: false, stopped: None, slow: 0, interval: None, last_pts: None }
    }

    /// Upscale one frame for a display box of `limit` (physical pixels),
    /// or pass it through; says which happened.
    pub fn process(&mut self, pts: f64, width: u32, height: u32, bgra: Vec<u8>, limit: Option<(u32, u32)>) -> ((u32, u32, Vec<u8>), Status) {
        if let Some(last) = self.last_pts.replace(pts) {
            let gap = pts - last;
            if gap > 0.0 && gap < 0.5 {
                self.interval = Some(self.interval.map_or(gap, |i| 0.9 * i + 0.1 * gap));
            }
        }
        let pass = |bgra: Vec<u8>, status: Status| ((width, height, bgra), status);
        if let Some(stopped) = &self.stopped {
            return pass(bgra, stopped.clone());
        }
        let Some((out_w, out_h)) = limit.and_then(|l| target_size((width, height), l)) else {
            return pass(bgra, Status::NotNeeded);
        };
        if !self.tried {
            self.tried = true;
            self.upscaler = (self.factory)();
        }
        let Some(upscaler) = self.upscaler.as_mut() else {
            let status = Status::Failed("no supported GPU".into());
            self.stopped = Some(status.clone());
            return pass(bgra, status);
        };
        let started = (self.clock)();
        match upscaler.upscale(&bgra, width, height, out_w, out_h) {
            Ok(out) => {
                let took = (self.clock)().duration_since(started).as_secs_f64();
                let budget = BUDGET_SHARE * self.interval.unwrap_or(1.0 / 30.0);
                self.slow = if took > budget { self.slow + 1 } else { 0 };
                if self.slow >= SLOW_FRAMES {
                    log::warn!("upscaling stopped for this file: {:.1} ms a frame, budget {:.1} ms", took * 1e3, budget * 1e3);
                    self.stopped = Some(Status::TooSlow);
                }
                ((out_w, out_h, out), Status::Active { from: (width, height), to: (out_w, out_h) })
            }
            Err(e) => {
                log::warn!("upscaling stopped for this file: {e:#}");
                let status = Status::Failed(format!("{e:#}"));
                self.stopped = Some(status.clone());
                pass(bgra, status)
            }
        }
    }
}

/// Peak signal-to-noise ratio between two same-sized BGRA images, in dB,
/// over the colour channels. For tests and tuning.
pub fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let (mut sum, mut n) = (0.0f64, 0usize);
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        for c in 0..3 {
            let d = pa[c] as f64 - pb[c] as f64;
            sum += d * d;
            n += 1;
        }
    }
    if sum == 0.0 { f64::INFINITY } else { 10.0 * (255.0f64 * 255.0 / (sum / n as f64)).log10() }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[test]
    fn frames_grow_to_fill_the_display_up_to_twice_their_size() {
        assert_eq!(target_size((1280, 720), (2560, 1440)), Some((2560, 1440)), "exactly 2x");
        assert_eq!(target_size((1280, 720), (3840, 2160)), Some((2560, 1440)), "3x away: capped at 2x");
        assert_eq!(target_size((1920, 1080), (2560, 1600)), Some((2560, 1440)), "fits the width, keeps the shape");
        assert_eq!(target_size((1920, 1080), (2000, 1200)), None, "4% bigger is not worth it");
        assert_eq!(target_size((3840, 2160), (1920, 1080)), None, "already bigger than the display");
        assert_eq!(target_size((0, 0), (1920, 1080)), None);
    }

    /// Doubles each pixel. Instead of taking real time it moves a fake
    /// clock on by `cost`, so timing tests do not depend on the machine.
    struct Fake {
        cost: Duration,
        now: Arc<Mutex<Instant>>,
        fail: bool,
    }

    impl Upscaler for Fake {
        fn upscale(&mut self, bgra: &[u8], w: u32, _h: u32, ow: u32, oh: u32) -> anyhow::Result<Vec<u8>> {
            *self.now.lock().unwrap() += self.cost;
            anyhow::ensure!(!self.fail, "device lost");
            let mut out = vec![0; (ow * oh * 4) as usize];
            for y in 0..oh {
                for x in 0..ow {
                    let s = (((y / 2) * w + x / 2) * 4) as usize;
                    let d = ((y * ow + x) * 4) as usize;
                    out[d..d + 4].copy_from_slice(&bgra[s..s + 4]);
                }
            }
            Ok(out)
        }
    }

    fn stage(cost: Duration, fail: bool) -> Stage {
        let now = Arc::new(Mutex::new(Instant::now()));
        let clock = now.clone();
        Stage::with_factory_and_clock(
            Box::new(move || Some(Box::new(Fake { cost, now: now.clone(), fail }) as Box<dyn Upscaler>)),
            Box::new(move || *clock.lock().unwrap()),
        )
    }

    #[test]
    fn a_stage_upscales_only_when_it_helps() {
        let mut s = stage(Duration::ZERO, false);
        let ((w, h, px), status) = s.process(0.0, 2, 2, vec![9; 16], Some((4, 4)));
        assert_eq!((w, h, px.len(), status), (4, 4, 64, Status::Active { from: (2, 2), to: (4, 4) }));
        let ((w, _, _), status) = s.process(0.04, 4, 4, vec![9; 64], Some((4, 4)));
        assert_eq!((w, status), (4, Status::NotNeeded));
        let ((w, _, _), status) = s.process(0.08, 2, 2, vec![9; 16], None);
        assert_eq!((w, status), (2, Status::NotNeeded), "no display size known yet");
    }

    #[test]
    fn a_stage_stops_for_the_file_when_it_cannot_keep_up() {
        // 25 ms a frame against a 40 ms frame interval: over the 50% budget.
        let mut s = stage(Duration::from_millis(25), false);
        let mut last = Status::Off;
        for i in 0..SLOW_FRAMES + 1 {
            last = s.process(i as f64 * 0.04, 2, 2, vec![0; 16], Some((4, 4))).1;
        }
        assert_eq!(last, Status::TooSlow);
        let ((w, _, _), status) = s.process(1.0, 2, 2, vec![0; 16], Some((4, 4)));
        assert_eq!((w, status), (2, Status::TooSlow), "frames pass through from then on");

        // The same cost against a 100 ms interval is fine.
        let mut s = stage(Duration::from_millis(25), false);
        for i in 0..SLOW_FRAMES + 1 {
            last = s.process(i as f64 * 0.1, 2, 2, vec![0; 16], Some((4, 4))).1;
        }
        assert!(matches!(last, Status::Active { .. }), "{last:?}");
    }

    #[test]
    fn a_stage_stops_for_the_file_on_a_gpu_error_or_no_gpu() {
        let mut s = stage(Duration::ZERO, true);
        let ((w, _, _), status) = s.process(0.0, 2, 2, vec![0; 16], Some((4, 4)));
        assert_eq!((w, status), (2, Status::Failed("device lost".into())));

        let mut s = Stage::with_factory(Box::new(|| None));
        assert_eq!(s.process(0.0, 2, 2, vec![0; 16], Some((4, 4))).1, Status::Failed("no supported GPU".into()));
    }

    /// A detailed test image: a zone plate (rings of rising frequency) with
    /// hard-edged diagonal bars, the kind of content upscalers differ on.
    pub(crate) fn test_image(w: u32, h: u32) -> Vec<u8> {
        let mut px = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f64 - w as f64 / 2.0, y as f64 - h as f64 / 2.0);
                let ring = 0.5 + 0.5 * ((fx * fx + fy * fy) * 0.0009).cos();
                let bar = if ((x + y) / 24) % 2 == 0 { 0.85 } else { 0.15 };
                let v = if x < w / 2 { ring } else { bar };
                let i = ((y * w + x) * 4) as usize;
                px[i] = (v * 255.0) as u8;
                px[i + 1] = (v * 230.0) as u8;
                px[i + 2] = (v * 200.0) as u8;
                px[i + 3] = 255;
            }
        }
        px
    }

    /// Halve an image by averaging 2x2 blocks.
    pub(crate) fn halve(px: &[u8], w: u32, h: u32) -> Vec<u8> {
        let (ow, oh) = (w / 2, h / 2);
        let mut out = vec![0u8; (ow * oh * 4) as usize];
        for y in 0..oh {
            for x in 0..ow {
                for c in 0..4 {
                    let at = |dx: u32, dy: u32| px[(((2 * y + dy) * w + 2 * x + dx) * 4 + c) as usize] as u32;
                    out[((y * ow + x) * 4 + c) as usize] = ((at(0, 0) + at(1, 0) + at(0, 1) + at(1, 1) + 2) / 4) as u8;
                }
            }
        }
        out
    }

    /// Bilinear upscaling: what the display does without an upscaler.
    pub(crate) fn bilinear(px: &[u8], w: u32, h: u32, ow: u32, oh: u32) -> Vec<u8> {
        let mut out = vec![0u8; (ow * oh * 4) as usize];
        for y in 0..oh {
            for x in 0..ow {
                let sx = ((x as f64 + 0.5) * w as f64 / ow as f64 - 0.5).clamp(0.0, (w - 1) as f64);
                let sy = ((y as f64 + 0.5) * h as f64 / oh as f64 - 0.5).clamp(0.0, (h - 1) as f64);
                let (x0, y0) = (sx.floor() as u32, sy.floor() as u32);
                let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
                let (tx, ty) = (sx - x0 as f64, sy - y0 as f64);
                for c in 0..4 {
                    let p = |xx: u32, yy: u32| px[((yy * w + xx) * 4 + c) as usize] as f64;
                    let top = p(x0, y0) * (1.0 - tx) + p(x1, y0) * tx;
                    let bot = p(x0, y1) * (1.0 - tx) + p(x1, y1) * tx;
                    out[((y * ow + x) * 4 + c) as usize] = (top * (1.0 - ty) + bot * ty).round() as u8;
                }
            }
        }
        out
    }

    /// An upscaler must beat bilinear scaling at restoring a halved image,
    /// keep a flat colour flat, and return the right size.
    pub(crate) fn check_quality(up: &mut dyn Upscaler) {
        let (w, h) = (512, 384);
        let full = test_image(w, h);
        let small = halve(&full, w, h);
        let out = up.upscale(&small, w / 2, h / 2, w, h).unwrap();
        assert_eq!(out.len(), full.len());
        let (ours, plain) = (psnr(&out, &full), psnr(&bilinear(&small, w / 2, h / 2, w, h), &full));
        eprintln!("PSNR: upscaler {ours:.2} dB, bilinear {plain:.2} dB");
        assert!(ours > plain + 0.3, "upscaler {ours:.2} dB is not clearly better than bilinear {plain:.2} dB");

        let flat: Vec<u8> = [40u8, 120, 200, 255].repeat(64 * 48);
        let out = up.upscale(&flat, 64, 48, 100, 75).unwrap();
        assert_eq!(out.len(), 100 * 75 * 4);
        assert!(out.as_chunks::<4>().0.iter().all(|p| p[0].abs_diff(40) <= 1 && p[1].abs_diff(120) <= 1 && p[2].abs_diff(200) <= 1), "flat colour changed");
    }
}
