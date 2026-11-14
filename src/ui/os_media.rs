//! OS media integration: hardware media keys, and the system "Now Playing"
//! surface (macOS Control Center, Windows media overlay, Linux MPRIS).

use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig};

/// What the OS should currently show.
#[derive(Clone, Debug, PartialEq)]
pub struct NowPlaying {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<f64>,
    pub state: PlayState,
    pub position: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayState {
    Stopped,
    Paused,
    Playing,
}

pub struct OsMedia {
    controls: MediaControls,
    events: Receiver<MediaControlEvent>,
    shown: Option<NowPlaying>,
    shown_at: Instant,
}

impl OsMedia {
    /// `hwnd` is required on Windows and ignored elsewhere.
    pub fn new(hwnd: Option<*mut std::ffi::c_void>) -> Option<Self> {
        let config = PlatformConfig { display_name: "airsplice-player", dbus_name: "airsplice_player", hwnd };
        let mut controls = MediaControls::new(config).map_err(|e| log::warn!("media controls unavailable: {e:?}")).ok()?;
        let (tx, events) = mpsc::channel();
        controls
            .attach(move |event| {
                let _ = tx.send(event);
            })
            .map_err(|e| log::warn!("media controls unavailable: {e:?}"))
            .ok()?;
        Some(Self { controls, events, shown: None, shown_at: Instant::now() })
    }

    pub fn poll(&self) -> Vec<MediaControlEvent> {
        self.events.try_iter().collect()
    }

    /// Push changes to the OS. Position is sent only on track or state
    /// changes and on jumps (seeks); the OS extrapolates it otherwise.
    pub fn update(&mut self, now: Option<NowPlaying>) {
        // Where the OS believes playback is now: it keeps the clock running
        // from the last position we sent.
        let predicted = self.shown.clone().map(|mut s| {
            if s.state == PlayState::Playing {
                s.position += self.shown_at.elapsed().as_secs_f64();
            }
            s
        });
        if !needs_update(predicted.as_ref(), now.as_ref()) {
            return;
        }
        let result = match &now {
            None => self.controls.set_playback(MediaPlayback::Stopped),
            Some(n) => {
                let same_track = self.shown.as_ref().is_some_and(|s| s.title == n.title && s.duration == n.duration);
                let metadata = if same_track {
                    Ok(())
                } else {
                    self.controls.set_metadata(MediaMetadata {
                        title: Some(&n.title),
                        artist: n.artist.as_deref(),
                        album: n.album.as_deref(),
                        duration: n.duration.map(Duration::from_secs_f64),
                        ..Default::default()
                    })
                };
                let progress = Some(MediaPosition(Duration::from_secs_f64(n.position.max(0.0))));
                metadata.and_then(|()| {
                    self.controls.set_playback(match n.state {
                        PlayState::Stopped => MediaPlayback::Stopped,
                        PlayState::Paused => MediaPlayback::Paused { progress },
                        PlayState::Playing => MediaPlayback::Playing { progress },
                    })
                })
            }
        };
        if let Err(e) = result {
            log::warn!("updating media controls: {e:?}");
        }
        self.shown = now;
        self.shown_at = Instant::now();
    }
}

/// Whether the OS view is stale: different track or state, or the position
/// is more than 1 s from where the OS predicts it (`shown` is predicted).
fn needs_update(shown: Option<&NowPlaying>, now: Option<&NowPlaying>) -> bool {
    match (shown, now) {
        (None, None) => false,
        (Some(s), Some(n)) => {
            if (&s.title, &s.artist, &s.album, s.duration, s.state) != (&n.title, &n.artist, &n.album, n.duration, n.state) {
                return true;
            }
            (n.position - s.position).abs() > 1.0
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn np(state: PlayState, position: f64) -> NowPlaying {
        NowPlaying { title: "T".into(), artist: None, album: None, duration: Some(60.0), state, position }
    }

    #[test]
    fn only_meaningful_changes_reach_the_os() {
        let shown = np(PlayState::Playing, 10.0);
        assert!(!needs_update(Some(&shown), Some(&np(PlayState::Playing, 10.5))), "drift within 1 s");
        assert!(needs_update(Some(&shown), Some(&np(PlayState::Playing, 30.0))), "seek");
        assert!(needs_update(Some(&shown), Some(&np(PlayState::Paused, 10.0))), "pause");
        assert!(needs_update(Some(&shown), None), "stop");
        assert!(!needs_update(None, None));
        let mut other = np(PlayState::Playing, 10.0);
        other.title = "U".into();
        assert!(needs_update(Some(&shown), Some(&other)), "track change");
    }
}
