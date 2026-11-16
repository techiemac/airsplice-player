//! Decides when the status bar warns that playback is not keeping up.
//!
//! The player counts lost video frames and audio dropouts as they happen;
//! this looks at those counts once per window and turns them into a warning
//! that neither flickers nor lingers: it appears after a window with a
//! visible stutter and goes after several clean windows in a row.

use std::time::Duration;

/// How often the counts are judged.
pub const WINDOW: Duration = Duration::from_secs(2);
/// Lost frames in one window that make a visible stutter. One or two can
/// be a one-off hiccup nobody notices.
const LOST_FRAMES: u64 = 3;
/// Clean windows in a row before the warning goes away.
const CLEAN_WINDOWS: u32 = 3;
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
    length: Duration,
}

impl Counts {
    fn trouble(&self) -> Option<Trouble> {
        let secs = self.length.as_secs_f64().max(0.001);
        let slow = f64::from(self.renders) / secs < SLOW_RENDERS_PER_SEC;
        let frames = self.lost_frames + if slow { u64::from(self.never_drawn) } else { 0 };
        if frames >= LOST_FRAMES {
            Some(Trouble::Frames)
        } else if self.dropouts > 0 {
            Some(Trouble::Audio)
        } else {
            None
        }
    }
}

pub struct PerfWatch {
    /// Running counts at the start of the window.
    lost_frames: u64,
    dropouts: u64,
    clean: u32,
    shown: Option<Trouble>,
}

impl Default for PerfWatch {
    fn default() -> Self {
        Self { lost_frames: 0, dropouts: 0, clean: CLEAN_WINDOWS, shown: None }
    }
}

impl PerfWatch {
    /// The warning to show, if any.
    pub fn shown(&self) -> Option<Trouble> {
        self.shown
    }

    /// Watch a different player (a new file, or a tab brought forward):
    /// nothing carries over, and its counts so far are not news.
    pub fn start(&mut self, lost_frames: u64, dropouts: u64) {
        *self = Self { lost_frames, dropouts, ..Self::default() };
    }

    /// Close a window, given the player's running counts and the render
    /// counts for the window. Returns true when the warning changed.
    pub fn close(&mut self, lost_frames: u64, dropouts: u64, never_drawn: u32, renders: u32, length: Duration) -> bool {
        let window = Counts {
            lost_frames: lost_frames.saturating_sub(self.lost_frames),
            dropouts: dropouts.saturating_sub(self.dropouts),
            never_drawn,
            renders,
            length,
        };
        self.lost_frames = lost_frames;
        self.dropouts = dropouts;
        let before = self.shown;
        match window.trouble() {
            Some(trouble) => {
                self.clean = 0;
                // Lost frames outrank audio: they are what is seen.
                if self.shown != Some(Trouble::Frames) {
                    self.shown = Some(trouble);
                }
            }
            None => {
                self.clean += 1;
                if self.clean >= CLEAN_WINDOWS {
                    self.shown = None;
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

    #[test]
    fn a_stutter_shows_the_warning_and_clean_windows_clear_it() {
        let mut p = PerfWatch::default();
        assert!(!p.close(0, 0, 0, 120, W));
        assert!(p.close(5, 0, 0, 120, W));
        assert_eq!(p.shown(), Some(Trouble::Frames));
        // Two clean windows: still shown, so it never flickers.
        assert!(!p.close(5, 0, 0, 120, W));
        assert!(!p.close(5, 0, 0, 120, W));
        assert_eq!(p.shown(), Some(Trouble::Frames));
        assert!(p.close(5, 0, 0, 120, W));
        assert_eq!(p.shown(), None);
    }

    #[test]
    fn a_trouble_window_restarts_the_clean_count() {
        let mut p = PerfWatch::default();
        p.close(3, 0, 0, 120, W);
        p.close(3, 0, 0, 120, W);
        p.close(3, 0, 0, 120, W);
        p.close(6, 0, 0, 120, W);
        p.close(6, 0, 0, 120, W);
        p.close(6, 0, 0, 120, W);
        assert_eq!(p.shown(), Some(Trouble::Frames));
    }

    #[test]
    fn a_one_off_hiccup_is_not_a_warning() {
        let mut p = PerfWatch::default();
        assert!(!p.close(2, 0, 0, 120, W));
        assert_eq!(p.shown(), None);
    }

    #[test]
    fn frames_never_drawn_count_only_when_rendering_is_slow() {
        let mut p = PerfWatch::default();
        // 120 fps video on a 60 Hz display: half never drawn, nothing lost.
        assert!(!p.close(0, 0, 120, 120, W));
        // The Raspberry Pi drawing 20 of 30 frames a second.
        assert!(p.close(0, 0, 20, 40, W));
        assert_eq!(p.shown(), Some(Trouble::Frames));
    }

    #[test]
    fn audio_dropouts_warn_and_lost_frames_take_precedence() {
        let mut p = PerfWatch::default();
        assert!(p.close(0, 1, 0, 120, W));
        assert_eq!(p.shown(), Some(Trouble::Audio));
        assert!(p.close(4, 2, 0, 120, W));
        assert_eq!(p.shown(), Some(Trouble::Frames));
        // Audio trouble alone does not demote it back.
        assert!(!p.close(4, 3, 0, 120, W));
        assert_eq!(p.shown(), Some(Trouble::Frames));
    }

    #[test]
    fn a_new_player_starts_clean_from_its_own_counts() {
        let mut p = PerfWatch::default();
        p.close(10, 0, 0, 120, W);
        // A tab brought forward that once lost 50 frames.
        p.start(50, 4);
        assert_eq!(p.shown(), None);
        assert!(!p.close(51, 4, 0, 120, W));
    }
}
