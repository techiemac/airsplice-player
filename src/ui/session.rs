//! Remembers the playlist, position, volume and layout between launches.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub playlist: Vec<PathBuf>,
    pub current: Option<usize>,
    pub position: f64,
    pub volume: f32,
    pub muted: bool,
    pub repeat: String,
    pub sidebar_open: bool,
    pub info_open: bool,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            playlist: Vec::new(),
            current: None,
            position: 0.0,
            volume: 0.8,
            muted: false,
            repeat: "off".into(),
            sidebar_open: true,
            info_open: false,
        }
    }
}

impl Session {
    /// Drop playlist entries that no longer exist, keeping `current`
    /// pointing at the same file (or clearing it if that file is gone).
    pub fn without_missing_files(mut self) -> Self {
        let current_path = self.current.and_then(|i| self.playlist.get(i).cloned());
        self.playlist.retain(|p| p.exists());
        self.current = current_path.and_then(|c| self.playlist.iter().position(|p| *p == c));
        if self.current.is_none() {
            self.position = 0.0;
        }
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

    /// `<config dir>/media-play/session.json` (e.g. `~/Library/Application
    /// Support` on macOS, `~/.config` on Linux, `%APPDATA%` on Windows).
    pub fn default_location() -> Option<Self> {
        Some(Self::new(dirs::config_dir()?.join("media-play").join("session.json")))
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
        let dir = std::env::temp_dir().join(format!("media-play-test-{name}-{}", std::process::id()));
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
            playlist: vec![track],
            current: Some(0),
            position: 12.5,
            volume: 0.3,
            muted: true,
            repeat: "all".into(),
            sidebar_open: false,
            info_open: true,
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
        let s = Session { playlist: vec![a.clone(), b.clone(), c.clone()], current: Some(2), position: 40.0, ..Default::default() }
            .without_missing_files();
        assert_eq!(s.playlist, vec![a.clone(), c.clone()]);
        assert_eq!((s.current, s.position), (Some(1), 40.0));

        let s = Session { playlist: vec![a, b, c], current: Some(1), position: 40.0, ..Default::default() }.without_missing_files();
        assert_eq!((s.current, s.position), (None, 0.0), "the current file itself is gone");
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
