//! Remembers the playlist, position, volume and layout between launches.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub playlist: Vec<PathBuf>,
    /// Each playlist entry's own dissolve length in seconds (0 cuts),
    /// from a playlist file; `None` follows the setting. Missing or short
    /// in sessions saved before entries had one.
    pub entry_dissolve: Vec<Option<f32>>,
    /// The playlist file last opened or saved, offered when saving again.
    pub playlist_file: Option<PathBuf>,
    pub current: Option<usize>,
    pub position: f64,
    pub volume: f32,
    pub muted: bool,
    pub repeat: String,
    pub sidebar_open: bool,
    pub info_open: bool,
    /// Open tabs, left to right. Empty in sessions saved before tabs
    /// existed; `current` and `position` then describe the one file open.
    pub tabs: Vec<TabState>,
    pub active_tab: Option<usize>,
    /// Folders added to the playlist; files added to them later join it.
    pub folders: Vec<PathBuf>,
    /// Files inside those folders that the user removed from the playlist,
    /// so a rescan does not bring them back. Sorted.
    pub removed_from_folders: Vec<PathBuf>,
    /// Stable ids of the output displays; the picture goes full screen on
    /// each one whenever it is connected.
    pub output_displays: Vec<String>,
    /// The outputs' on/off switch. On by default, so choosing a display
    /// in the settings shows the picture there straight away.
    pub outputs_on: bool,
    /// Stable id of the audio output device; `None` follows the system's
    /// default output.
    pub audio_output: Option<String>,
    /// Upscale video on the GPU to the size it is shown at.
    pub upscaling: bool,
    /// Seconds a still image shows before the playlist moves on; 0 keeps
    /// it up until skipped.
    pub still_seconds: u32,
    /// Dissolve from one playlist entry into the next, over
    /// `dissolve_seconds`.
    pub dissolve: bool,
    pub dissolve_seconds: f32,
    /// Move on to the next playlist entry when one ends.
    pub autoplay: bool,
    /// Folders collapsed in the playlist.
    pub collapsed_folders: Vec<PathBuf>,
    /// The playlist pane's width, in logical pixels.
    pub sidebar_width: f32,
}

/// One open tab: its file and where it was left.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TabState {
    pub path: PathBuf,
    pub position: f64,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            playlist: Vec::new(),
            entry_dissolve: Vec::new(),
            playlist_file: None,
            current: None,
            position: 0.0,
            volume: 0.8,
            muted: false,
            repeat: "off".into(),
            sidebar_open: true,
            info_open: false,
            tabs: Vec::new(),
            active_tab: None,
            folders: Vec::new(),
            removed_from_folders: Vec::new(),
            output_displays: Vec::new(),
            outputs_on: true,
            audio_output: None,
            upscaling: false,
            still_seconds: 10,
            dissolve: false,
            dissolve_seconds: 1.0,
            autoplay: true,
            collapsed_folders: Vec::new(),
            sidebar_width: 240.0,
        }
    }
}

impl Session {
    /// Drop playlist entries and tabs whose files no longer exist, keeping
    /// `current` and `active_tab` pointing at the same file (or clearing
    /// them if that file is gone).
    pub fn without_missing_files(mut self) -> Self {
        let active_path = self.active_tab.and_then(|i| self.tabs.get(i)).map(|t| t.path.clone());
        self.tabs.retain(|t| t.path.exists());
        for t in &mut self.tabs {
            if !t.position.is_finite() || t.position < 0.0 {
                t.position = 0.0;
            }
        }
        self.active_tab = active_path.and_then(|a| self.tabs.iter().position(|t| t.path == a));
        if self.active_tab.is_none() && !self.tabs.is_empty() {
            self.active_tab = Some(0);
        }
        let current_path = self.current.and_then(|i| self.playlist.get(i).cloned());
        self.entry_dissolve.resize(self.playlist.len(), None);
        let kept: Vec<bool> = self.playlist.iter().map(|p| p.exists()).collect();
        let mut keep = kept.iter();
        self.playlist.retain(|_| *keep.next().unwrap());
        let mut keep = kept.iter();
        self.entry_dissolve.retain(|_| *keep.next().unwrap());
        for d in self.entry_dissolve.iter_mut() {
            *d = d.filter(|s| s.is_finite() && *s >= 0.0);
        }
        self.current = current_path.and_then(|c| self.playlist.iter().position(|p| *p == c));
        if self.current.is_none() {
            self.position = 0.0;
        }
        self.folders.retain(|f| f.is_dir());
        self.removed_from_folders.retain(|p| p.exists());
        self.volume = self.volume.clamp(0.0, 1.0);
        if !self.position.is_finite() || self.position < 0.0 {
            self.position = 0.0;
        }
        self
    }
}

/// Where the session lives on disk.
#[derive(Clone, Debug)]
pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// `<config dir>/airsplice-player/session.json` (e.g. `~/Library/Application
    /// Support` on macOS, `~/.config` on Linux, `%APPDATA%` on Windows).
    pub fn default_location() -> Option<Self> {
        Some(Self::in_config_dir(&dirs::config_dir()?))
    }

    /// The session file under `config`. A session saved under the player's
    /// former name (`media-play`) is copied over the first time, so renaming
    /// the app does not lose the user's playlist. The old file is left alone.
    fn in_config_dir(config: &Path) -> Self {
        let store = Self::new(config.join("airsplice-player").join("session.json"));
        let legacy = config.join("media-play").join("session.json");
        if !store.path.exists() && legacy.is_file() {
            let copied = store.path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::copy(&legacy, &store.path));
            if let Err(e) = copied {
                log::warn!("could not copy session from {}: {e}", legacy.display());
            }
        }
        store
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A missing file is a fresh start; an unreadable one is logged and
    /// treated the same way rather than blocking launch.
    pub fn load(&self) -> Session {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice::<Session>(&bytes).unwrap_or_else(|e| {
                log::warn!("ignoring unreadable session {}: {e}", self.path.display());
                Session::default()
            }),
            Err(_) => Session::default(),
        }
        .without_missing_files()
    }

    /// Write atomically: a crash mid-write never leaves a truncated file.
    pub fn save(&self, session: &Session) -> Result<()> {
        let dir = self.path.parent().context("session path has no parent")?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(session)?).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path).with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("airsplice-player-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trips_and_creates_its_directory() {
        let dir = temp_dir("roundtrip");
        let store = SessionStore::new(dir.join("nested").join("session.json"));
        let track = dir.join("a.mp3");
        std::fs::write(&track, b"x").unwrap();
        let session = Session {
            playlist: vec![track.clone()],
            entry_dissolve: vec![Some(2.5)],
            playlist_file: Some(dir.join("show.airsplice")),
            current: Some(0),
            position: 12.5,
            volume: 0.3,
            muted: true,
            repeat: "all".into(),
            sidebar_open: false,
            info_open: true,
            tabs: vec![TabState { path: track.clone(), position: 12.5 }],
            active_tab: Some(0),
            folders: vec![dir.clone()],
            removed_from_folders: vec![track],
            output_displays: vec!["6f1c-display".into(), "a07e-display".into()],
            outputs_on: false,
            audio_output: Some("coreaudio:BuiltInSpeakerDevice".into()),
            upscaling: true,
            still_seconds: 45,
            dissolve: true,
            dissolve_seconds: 2.5,
            autoplay: false,
            collapsed_folders: vec![dir.clone()],
            sidebar_width: 320.0,
        };
        store.save(&session).unwrap();
        assert_eq!(store.load(), session);
        assert!(!store.path().with_extension("json.tmp").exists(), "temp file renamed away");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_files_are_dropped_and_current_follows_its_file() {
        let dir = temp_dir("missing");
        let (a, b, c) = (dir.join("a.mp3"), dir.join("gone.mp3"), dir.join("c.mp3"));
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&c, b"x").unwrap();
        let s = Session {
            playlist: vec![a.clone(), b.clone(), c.clone()],
            entry_dissolve: vec![None, Some(1.0), Some(f32::NAN)],
            current: Some(2),
            position: 40.0,
            ..Default::default()
        }
        .without_missing_files();
        assert_eq!(s.playlist, vec![a.clone(), c.clone()]);
        assert_eq!(s.entry_dissolve, vec![None, None], "kept in step with the playlist, bad lengths dropped");
        assert_eq!((s.current, s.position), (Some(1), 40.0));

        let s = Session { playlist: vec![a, b, c], current: Some(1), position: 40.0, ..Default::default() }.without_missing_files();
        assert_eq!((s.current, s.position), (None, 0.0), "the current file itself is gone");
        assert_eq!(s.entry_dissolve, vec![None, None], "an older session without lengths");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_session_under_the_former_name_is_carried_over_once() {
        let config = temp_dir("legacy");
        let legacy = config.join("media-play").join("session.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, br#"{"volume": 0.25}"#).unwrap();

        let store = SessionStore::in_config_dir(&config);
        assert_eq!(store.path(), config.join("airsplice-player").join("session.json"));
        assert_eq!(store.load().volume, 0.25, "copied from the old location");
        assert!(legacy.exists(), "the old file is left in place");

        // Once the new file exists it wins, even if the old one changes.
        store.save(&Session { volume: 0.5, ..Default::default() }).unwrap();
        std::fs::write(&legacy, br#"{"volume": 0.75}"#).unwrap();
        assert_eq!(SessionStore::in_config_dir(&config).load().volume, 0.5);

        // No old session: nothing is created until the first save.
        let fresh = temp_dir("fresh");
        assert!(!SessionStore::in_config_dir(&fresh).path().exists());
        let _ = std::fs::remove_dir_all(config);
        let _ = std::fs::remove_dir_all(fresh);
    }

    #[test]
    fn tabs_whose_files_are_gone_are_dropped_and_the_active_tab_follows() {
        let dir = temp_dir("tabs");
        let (a, gone, c) = (dir.join("a.mp4"), dir.join("gone.mp4"), dir.join("c.mp4"));
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&c, b"x").unwrap();
        let tab = |path: &PathBuf, position: f64| TabState { path: path.clone(), position };
        let s = Session { tabs: vec![tab(&a, 1.0), tab(&gone, 2.0), tab(&c, f64::NAN)], active_tab: Some(2), ..Default::default() }
            .without_missing_files();
        assert_eq!(s.tabs, vec![tab(&a, 1.0), tab(&c, 0.0)], "missing file dropped, bad position reset");
        assert_eq!(s.active_tab, Some(1), "still the same file");

        let s = Session { tabs: vec![tab(&a, 1.0), tab(&gone, 2.0)], active_tab: Some(1), ..Default::default() }.without_missing_files();
        assert_eq!(s.active_tab, Some(0), "the active tab's file is gone: the first tab instead");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn folders_that_are_gone_are_dropped() {
        let dir = temp_dir("folders");
        let gone = dir.join("unplugged");
        let s = Session { folders: vec![dir.clone(), gone.clone()], removed_from_folders: vec![gone.join("a.mp4")], ..Default::default() }
            .without_missing_files();
        assert_eq!((s.folders, s.removed_from_folders), (vec![dir.clone()], vec![]));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_or_partial_files_fall_back_to_defaults() {
        let dir = temp_dir("corrupt");
        let store = SessionStore::new(dir.join("session.json"));
        std::fs::write(store.path(), b"{ not json").unwrap();
        assert_eq!(store.load(), Session::default());
        // Older or partial files keep what they have and default the rest.
        std::fs::write(store.path(), br#"{"volume": 0.25, "position": -3}"#).unwrap();
        let s = store.load();
        assert_eq!((s.volume, s.position, s.sidebar_open), (0.25, 0.0, true));
        let _ = std::fs::remove_dir_all(dir);
    }
}
