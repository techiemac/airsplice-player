//! Decides when the status bar warns that playback is not keeping up.
//!
//! The player counts lost video frames and audio dropouts as they happen;
//! this looks at those counts once per window and turns them into a warning
//! that neither flickers nor lingers: it appears after a window with a
//! visible stutter and goes once playback has been free of trouble for a
//! while.

use std::time::Duration;

/// How often the counts are judged.
pub const WINDOW: Duration = Duration::from_secs(2);
/// Lost frames in one window that make a visible stutter. One or two can
/// be a one-off hiccup nobody notices.
const LOST_FRAMES: u64 = 3;
/// How long after the last trouble the warning goes away.
const LINGER: Duration = Duration::from_secs(12);
/// Below this many renders a second, frames replaced before they were
/// drawn are the renderer falling behind. Above it they are video faster
/// than the display (120 fps on a 60 Hz screen), which loses nothing.
const SLOW_RENDERS_PER_SEC: f64 = 45.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trouble {
    /// Video frames are not making it to the screen.
    Frames,
    /// The audio is running dry.
    Audio,
}

impl Trouble {
    pub fn label(self) -> &'static str {
        match self {
            Trouble::Frames => "Dropping frames",
            Trouble::Audio => "Audio dropouts",
        }
    }
}

/// What happened during one window.
#[derive(Clone, Copy, Debug, Default)]
struct Counts {
    /// Frames the player lost (its running count's increase).
    lost_frames: u64,
    /// Audio dropouts (the running count's increase).
    dropouts: u64,
    /// Frames taken from the player but replaced before any render.
    never_drawn: u32,
    /// Renders in the window.
    renders: u32,
    /// How far the newest decoded frame trailed the clock at the close, if
    /// the picture was frozen.
    behind: Option<f64>,
    length: Duration,
}

/// The numbers behind the warning, from the latest window with trouble.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Detail {
    /// Frames missed, counting those never drawn when rendering is slow.
    pub frames: u64,
    pub behind: Option<f64>,
    pub dropouts: u64,
    pub renders_per_sec: f64,
    pub length: Duration,
}

impl Detail {
    /// One line per kind of trouble, then the render rate when it is low.
    pub fn lines(&self) -> Vec<String> {
        let secs = self.length.as_secs_f64().round();
        let mut lines = Vec::new();
        if self.frames > 0 {
            lines.push(format!("{} {} missed in {secs:.0} s", self.frames, plural(self.frames, "frame", "frames")));
        }
        if let Some(behind) = self.behind {
            lines.push(format!("Video {behind:.1} s behind"));
        }
        if self.dropouts > 0 {
            lines.push(format!("Audio ran dry {} {} in {secs:.0} s", self.dropouts, plural(self.dropouts, "time", "times")));
        }
        if self.renders_per_sec < SLOW_RENDERS_PER_SEC {
            lines.push(format!("Screen redrawn {:.0} times a second", self.renders_per_sec));
        }
        lines
    }
}

fn plural(n: u64, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

impl Counts {
    fn renders_per_sec(&self) -> f64 {
        f64::from(self.renders) / self.length.as_secs_f64().max(0.001)
    }

    fn frames(&self) -> u64 {
        let slow = self.renders_per_sec() < SLOW_RENDERS_PER_SEC;
        self.lost_frames + if slow { u64::from(self.never_drawn) } else { 0 }
    }

    fn trouble(&self) -> Option<Trouble> {
        if self.frames() >= LOST_FRAMES || self.behind.is_some() {
            Some(Trouble::Frames)
        } else if self.dropouts > 0 {
            Some(Trouble::Audio)
        } else {
            None
        }
    }
}

/// What the player reports when a window closes.
#[derive(Clone, Copy, Debug, Default)]
pub struct Totals {
    /// Running count of lost video frames.
    pub lost_frames: u64,
    /// Running count of audio dropouts.
    pub dropouts: u64,
    /// How far the video trails the clock now, if the picture is frozen.
    pub behind: Option<f64>,
    /// The user seeked during the window, or playback is still reaching
    /// the last seek's target. Scrubbing skips and holds frames by design,
    /// so such a window says nothing either way.
    pub seeking: bool,
}

pub struct PerfWatch {
    /// Running counts at the start of the window.
    lost_frames: u64,
    dropouts: u64,
    /// Time since the last window with trouble.
    quiet: Duration,
    shown: Option<Trouble>,
    detail: Option<Detail>,
}

impl Default for PerfWatch {
    fn default() -> Self {
        Self { lost_frames: 0, dropouts: 0, quiet: LINGER, shown: None, detail: None }
    }
}

impl PerfWatch {
    /// The warning to show, if any.
    pub fn shown(&self) -> Option<Trouble> {
        self.shown
    }

    /// The numbers behind the warning shown, for its tooltip.
    pub fn detail(&self) -> Option<Detail> {
        self.shown.and(self.detail)
    }

    /// Watch a different player (a new file, or a tab brought forward):
    /// nothing carries over, and its counts so far are not news.
    pub fn start(&mut self, lost_frames: u64, dropouts: u64) {
        *self = Self { lost_frames, dropouts, ..Self::default() };
    }

    /// Close a window, given the player's running counts and the render
    /// counts for the window. Returns true when the warning changed.
    pub fn close(&mut self, player: Totals, never_drawn: u32, renders: u32, length: Duration) -> bool {
        let window = Counts {
            lost_frames: player.lost_frames.saturating_sub(self.lost_frames),
            dropouts: player.dropouts.saturating_sub(self.dropouts),
            never_drawn,
            renders,
            behind: player.behind,
            length,
        };
        let Totals { lost_frames, dropouts, seeking, .. } = player;
        self.lost_frames = lost_frames;
        self.dropouts = dropouts;
        let before = self.shown;
        // A seek's window is judged neither way, but its time still counts
        // towards the warning going, so scrubbing never holds it up.
        match window.trouble().filter(|_| !seeking) {
            Some(trouble) => {
                log::debug!(
                    "playback trouble: {} lost, {} never drawn at {:.0} renders/s, behind {:?}, {} audio dropouts",
                    window.lost_frames,
                    window.never_drawn,
                    window.renders_per_sec(),
                    window.behind,
                    window.dropouts
                );
                self.quiet = Duration::ZERO;
                self.detail = Some(Detail {
                    frames: window.frames(),
                    behind: window.behind,
                    dropouts: window.dropouts,
                    renders_per_sec: window.renders_per_sec(),
                    length: window.length,
                });
                // Lost frames outrank audio: they are what is seen.
                if self.shown != Some(Trouble::Frames) {
                    self.shown = Some(trouble);
                }
            }
            None => {
                self.quiet += length;
                if self.quiet >= LINGER {
                    self.shown = None;
                    self.detail = None;
                }
            }
        }
        self.shown != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Duration = WINDOW;

    fn t(lost_frames: u64, dropouts: u64) -> Totals {
        Totals { lost_frames, dropouts, behind: None, seeking: false }
    }

    /// Close `n` clean windows.
    fn quiet(p: &mut PerfWatch, n: u32) {
        for _ in 0..n {
            p.close(Totals { lost_frames: p.lost_frames, dropouts: p.dropouts, ..t(0, 0) }, 0, 120, W);
        }
    }

    #[test]
    fn a_stutter_shows_the_warning_for_twelve_seconds() {
        let mut p = PerfWatch::default();
        assert!(!p.close(t(0, 0), 0, 120, W));
        assert!(p.close(t(5, 0), 0, 120, W));
        assert_eq!(p.shown(), Some(Trouble::Frames));
        // Ten seconds clean: still shown, so it never flickers.
        quiet(&mut p, 5);
        assert_eq!(p.shown(), Some(Trouble::Frames));
        assert!(p.close(t(5, 0), 0, 120, W));
        assert_eq!(p.shown(), None);
    }

    #[test]
    fn more_trouble_restarts_the_twelve_seconds() {
        let mut p = PerfWatch::default();
        p.close(t(3, 0), 0, 120, W);
        quiet(&mut p, 4);
        p.close(t(6, 0), 0, 120, W);
        quiet(&mut p, 5);
        assert_eq!(p.shown(), Some(Trouble::Frames));
    }

    #[test]
    fn a_one_off_hiccup_is_not_a_warning() {
        let mut p = PerfWatch::default();
        assert!(!p.close(t(2, 0), 0, 120, W));
        assert_eq!(p.shown(), None);
    }

    #[test]
    fn frames_never_drawn_count_only_when_rendering_is_slow() {
        let mut p = PerfWatch::default();
        // 120 fps video on a 60 Hz display: half never drawn, nothing lost.
        assert!(!p.close(t(0, 0), 120, 120, W));
        // The Raspberry Pi drawing 20 of 30 frames a second.
        assert!(p.close(t(0, 0), 20, 40, W));
        assert_eq!(p.shown(), Some(Trouble::Frames));
    }

    #[test]
    fn audio_dropouts_warn_and_lost_frames_take_precedence() {
        let mut p = PerfWatch::default();
        assert!(p.close(t(0, 1), 0, 120, W));
        assert_eq!(p.shown(), Some(Trouble::Audio));
        assert!(p.close(t(4, 2), 0, 120, W));
        assert_eq!(p.shown(), Some(Trouble::Frames));
        // Audio trouble alone does not demote it back.
        assert!(!p.close(t(4, 3), 0, 120, W));
        assert_eq!(p.shown(), Some(Trouble::Frames));
    }

    #[test]
    fn the_detail_is_the_latest_window_with_trouble() {
        let mut p = PerfWatch::default();
        assert_eq!(p.detail(), None);
        // The Raspberry Pi drawing 20 of 30 frames a second, and the audio
        // running dry once.
        p.close(t(4, 1), 20, 40, W);
        assert_eq!(p.detail().unwrap().lines(), ["24 frames missed in 2 s", "Audio ran dry 1 time in 2 s", "Screen redrawn 20 times a second"]);
        // Clean windows keep the numbers that caused the warning.
        quiet(&mut p, 5);
        assert_eq!(p.detail().unwrap().lines(), ["24 frames missed in 2 s", "Audio ran dry 1 time in 2 s", "Screen redrawn 20 times a second"]);
        quiet(&mut p, 1);
        assert_eq!(p.detail(), None);
    }

    #[test]
    fn a_frozen_picture_warns_though_nothing_was_dropped() {
        let mut p = PerfWatch::default();
        // A decoder too slow to get as far as dropping a frame.
        assert!(p.close(Totals { behind: Some(3.24), ..t(0, 0) }, 0, 120, W));
        assert_eq!(p.shown(), Some(Trouble::Frames));
        assert_eq!(p.detail().unwrap().lines(), ["Video 3.2 s behind"]);
    }

    #[test]
    fn scrubbing_never_warns_nor_holds_a_warning_up() {
        let mut p = PerfWatch::default();
        let seeking = |lost| Totals { seeking: true, behind: Some(1.0), ..t(lost, 1) };
        // Scrubbing: frames skipped, the picture held, audio restarted.
        assert!(!p.close(seeking(40), 0, 30, W));
        assert_eq!(p.shown(), None);
        // What was lost while scrubbing is not carried into the next window.
        assert!(!p.close(t(40, 1), 0, 120, W));
        assert_eq!(p.shown(), None);
        // A warning from before goes on time, scrubbing or not.
        p.close(t(45, 1), 0, 120, W);
        for lost in [50, 60, 70, 80, 90] {
            p.close(seeking(lost), 0, 30, W);
        }
        assert_eq!(p.shown(), Some(Trouble::Frames));
        assert!(p.close(seeking(100), 0, 30, W));
        assert_eq!(p.shown(), None);
    }

    #[test]
    fn a_new_player_starts_clean_from_its_own_counts() {
        let mut p = PerfWatch::default();
        p.close(t(10, 0), 0, 120, W);
        // A tab brought forward that once lost 50 frames.
        p.start(50, 4);
        assert_eq!(p.shown(), None);
        assert!(!p.close(t(51, 4), 0, 120, W));
    }
}
