//! The optional dissolve between playlist entries.
//!
//! When it is on, moving to another entry (autoplay, or picking one) does
//! not close the file on screen: it keeps playing underneath while the new
//! one fades in over it, and their sound crossfades. Autoplay starts the
//! next entry one dissolve before the current one ends, so the two overlap
//! instead of the new one fading in over a stopped picture. Tab switches
//! stay cuts.

use super::*;

pub(super) const MIN_DISSOLVE: f32 = 0.5;
pub(super) const MAX_DISSOLVE: f32 = 5.0;
const DISSOLVE_STEP: f32 = 0.5;

/// `secs` moved by `by` steps of half a second, within the limits.
pub(super) fn step_dissolve(secs: f32, by: i64) -> f32 {
    let steps = (secs / DISSOLVE_STEP).round() as i64 + by;
    (steps as f32 * DISSOLVE_STEP).clamp(MIN_DISSOLVE, MAX_DISSOLVE)
}

pub(super) fn dissolve_label(secs: f32) -> String {
    if secs.fract() == 0.0 { format!("{secs:.0} s") } else { format!("{secs:.1} s") }
}

/// How long the dissolve waits for the new entry's first picture (a large
/// image can take a moment) before it starts anyway.
const WAIT_FOR_PICTURE: Duration = Duration::from_secs(1);

/// The entry being dissolved away from.
pub(super) struct Outgoing {
    pub(super) loaded: Loaded,
    /// When the fade began: once the new entry has a picture to fade to.
    started: Option<Instant>,
    since: Instant,
}

/// Sound levels (outgoing, incoming) at `progress`: an equal-power
/// crossfade, so the overall loudness holds steady through it.
fn gains(progress: f32) -> (f32, f32) {
    let angle = progress.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2;
    (angle.cos(), angle.sin())
}

impl MediaPlayer {
    /// Take what is on screen to dissolve from, when the dissolve is on and
    /// something is open. A dissolve already under way ends first.
    pub(super) fn dissolve_from(&mut self, window: &mut Window) -> Option<Loaded> {
        self.end_dissolve(window);
        if !self.dissolve || self.player.is_none() {
            return None;
        }
        Some(self.take_installed())
    }

    /// Start dissolving from `from` into what is now on screen. If the new
    /// entry could not be opened, its error shows at once instead.
    pub(super) fn begin_dissolve(&mut self, from: Loaded, window: &mut Window) {
        if self.player.is_none() {
            from.release(window);
            return;
        }
        self.outgoing = Some(Outgoing { loaded: from, started: None, since: Instant::now() });
    }

    /// Finish any dissolve now: the outgoing file closes and the new one
    /// plays at full volume.
    pub(super) fn end_dissolve(&mut self, window: &mut Window) {
        if let Some(out) = self.outgoing.take() {
            out.loaded.release(window);
            if let Some(p) = &self.player {
                p.set_volume(self.volume, self.muted);
            }
        }
    }

    /// The volume a file starts at: silent when it is about to fade in.
    pub(super) fn entry_volume(&self) -> f32 {
        if self.fading_in { 0.0 } else { self.volume }
    }

    /// How far the dissolve has got, 0 to 1; `None` when there is none.
    pub(super) fn dissolve_progress(&self) -> Option<f32> {
        let out = self.outgoing.as_ref()?;
        Some(out.started.map_or(0.0, |t| (t.elapsed().as_secs_f32() / self.dissolve_secs).min(1.0)))
    }

    /// Called every tick: moves the outgoing picture and both volumes on,
    /// and ends the dissolve when it is complete. True while one is under
    /// way, so the window redraws.
    pub(super) fn tick_dissolve(&mut self, window: &mut Window) -> bool {
        let incoming_ready = match &self.player {
            Some(p) => self.frame.is_some() || !p.shows_picture(),
            None => true,
        };
        let Some(out) = self.outgoing.as_mut() else { return false };
        if let Some(frame) = out.loaded.player.as_ref().and_then(Player::take_frame)
            && let Some(old) = out.loaded.frame.replace(to_render_image(frame.width, frame.height, frame.bgra))
        {
            let _ = window.drop_image(old);
        }
        if out.started.is_none() && (incoming_ready || out.since.elapsed() >= WAIT_FOR_PICTURE) {
            out.started = Some(Instant::now());
        }
        let progress = self.dissolve_progress().unwrap_or(1.0);
        if progress >= 1.0 {
            self.end_dissolve(window);
            return true;
        }
        let (gone, coming) = gains(progress);
        if let Some(out) = self.outgoing.as_ref().and_then(|o| o.loaded.player.as_ref()) {
            out.set_volume(self.volume * gone, self.muted);
        }
        if let Some(p) = &self.player {
            p.set_volume(self.volume * coming, self.muted);
        }
        true
    }

    /// Whether autoplay should move on now, one dissolve before the end of
    /// the entry on screen. Entries shorter than two dissolves play out.
    pub(super) fn dissolve_due(&self) -> bool {
        let Some(p) = &self.player else { return false };
        let secs = self.dissolve_secs as f64;
        self.dissolve
            && self.outgoing.is_none()
            && !self.ended
            && !p.is_paused()
            && self.upcoming().is_some()
            && p.info.duration.is_some_and(|d| d >= 2.0 * secs && p.position() >= d - secs)
    }

    /// The pictures to draw, bottom first, each with its opacity: during a
    /// dissolve, the outgoing picture under the incoming one. With
    /// `covers`, cover art counts as a picture (the output windows show it
    /// full screen; the main window shows it beside the title).
    pub(in crate::ui) fn layers(&self, covers: bool) -> Vec<(Arc<RenderImage>, f32)> {
        let pick = |frame: &Option<Arc<RenderImage>>, cover: &Option<Arc<RenderImage>>| {
            frame.clone().or_else(|| if covers { cover.clone() } else { None })
        };
        let incoming = pick(&self.frame, &self.cover);
        let outgoing = self.outgoing.as_ref().and_then(|o| pick(&o.loaded.frame, &o.loaded.cover));
        match (outgoing, incoming, self.dissolve_progress()) {
            // Not started: the new entry's picture is not up yet.
            (Some(out), _, Some(p)) if p <= 0.0 => vec![(out, 1.0)],
            (Some(out), Some(inc), Some(p)) => vec![(out, 1.0), (inc, p)],
            (_, inc, _) => inc.into_iter().map(|i| (i, 1.0)).collect(),
        }
    }

    /// Turn the dissolve on or off. Remembered.
    pub(super) fn set_dissolve(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.dissolve = on;
        if !on {
            self.end_dissolve(window);
        }
        cx.notify();
    }

    pub(super) fn set_dissolve_secs(&mut self, secs: f32, cx: &mut Context<Self>) {
        self.dissolve_secs = secs.clamp(MIN_DISSOLVE, MAX_DISSOLVE);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_step_by_half_seconds_within_the_limits() {
        assert_eq!(step_dissolve(1.0, 1), 1.5);
        assert_eq!(step_dissolve(1.0, -1), 0.5);
        assert_eq!(step_dissolve(0.5, -1), 0.5);
        assert_eq!(step_dissolve(5.0, 1), 5.0);
        // A stored value off the grid lands back on it.
        assert_eq!(step_dissolve(1.3, 1), 2.0);
        assert_eq!((dissolve_label(1.0), dissolve_label(1.5)), ("1 s".to_string(), "1.5 s".to_string()));
    }

    #[test]
    fn the_crossfade_keeps_the_loudness_steady() {
        assert_eq!(gains(0.0), (1.0, 0.0));
        let (a, b) = gains(1.0);
        assert!(a.abs() < 1e-6 && (b - 1.0).abs() < 1e-6);
        for p in [0.1, 0.25, 0.5, 0.9] {
            let (a, b) = gains(p);
            assert!((a * a + b * b - 1.0).abs() < 1e-5, "power at {p}");
        }
    }
}
