//! Opening and saving `.airsplice` playlist files (see `crates/file`).
//!
//! Opening one adds its entries to the playlist, each with the dissolve
//! length the file gives it. Saving writes the playlist with those lengths.
//! Saving over an existing file keeps what other tools wrote in it.

use airsplice_file::{Document, Entry, Error, Playlist, is_airsplice_file, relative_path};

use super::*;

/// `paths` with each playlist file replaced by its entries, each with its
/// dissolve length; and what went wrong reading them, for the status bar.
pub(super) fn expand(paths: &[PathBuf]) -> (Vec<(PathBuf, Option<f32>)>, Vec<String>) {
    let mut items = Vec::new();
    let mut problems = Vec::new();
    for path in paths {
        if !is_airsplice_file(path) {
            items.push((path.clone(), None));
            continue;
        }
        let name = file_name(path);
        let doc = match Document::read(path) {
            Ok(doc) => doc,
            Err(e) => {
                problems.push(format!("{name}: {e}"));
                continue;
            }
        };
        let Some(playlist) = doc.player.and_then(|p| p.playlist) else {
            problems.push(format!("{name}: no playlist in it"));
            continue;
        };
        let folder = path.parent().unwrap_or(Path::new(""));
        let mut missing = 0;
        for entry in &playlist.entries {
            let file = entry.resolve(folder);
            if file.exists() {
                items.push((file, playlist.dissolve_into(entry).map(|s| s as f32)));
            } else {
                missing += 1;
            }
        }
        match missing {
            0 => {}
            1 => problems.push(format!("{name}: 1 entry not found")),
            n => problems.push(format!("{name}: {n} entries not found")),
        }
    }
    (items, problems)
}

/// An entry's own dissolve length, as the playlist shows it.
pub(super) fn entry_dissolve_label(secs: f32) -> String {
    if secs == 0.0 {
        return "Cut".into();
    }
    let rounded = (secs * 100.0).round() / 100.0;
    format!("{rounded} s")
}

/// Seconds as written to a file: to the millisecond, so a length set as
/// 0.1 is not written as 0.10000000149.
fn file_seconds(secs: f32) -> f64 {
    (secs as f64 * 1000.0).round() / 1000.0
}

impl MediaPlayer {
    pub(super) fn save_playlist(&mut self, _: &SavePlaylist, window: &mut Window, cx: &mut Context<Self>) {
        if self.playlist.is_empty() {
            self.notice = Some("The playlist is empty".into());
            cx.notify();
            return;
        }
        let (folder, name) = match &self.playlist_file {
            Some(file) => (file.parent().map(Path::to_path_buf), file_name(file)),
            None => (self.playlist[0].parent().map(Path::to_path_buf), format!("Playlist.{}", airsplice_file::EXTENSION)),
        };
        let folder = folder.filter(|f| f.is_dir()).or_else(dirs::home_dir).unwrap_or_default();
        let rx = cx.prompt_for_new_path(&folder, Some(&name));
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    this.save_playlist_to(&path);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Write the playlist to `path`, adding the extension when it has none.
    pub(super) fn save_playlist_to(&mut self, path: &Path) {
        match self.write_playlist(path) {
            Ok(saved) => {
                self.playlist_file = Some(saved);
                self.notice = None;
            }
            Err(e) => self.notice = Some(format!("Not saved: {e}")),
        }
    }

    fn write_playlist(&self, path: &Path) -> Result<PathBuf, String> {
        let path = if is_airsplice_file(path) {
            path.to_path_buf()
        } else {
            let mut named = path.as_os_str().to_owned();
            named.push(".");
            named.push(airsplice_file::EXTENSION);
            PathBuf::from(named)
        };
        let name = file_name(&path);
        let folder = path.parent().ok_or_else(|| format!("{name} has no folder"))?;
        // Playlist paths are canonical; so is the folder they are made
        // relative to.
        let folder = std::fs::canonicalize(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
        let path = folder.join(path.file_name().ok_or_else(|| format!("{name} has no name"))?);
        // Over an existing file, what other tools wrote in it stays. One
        // that cannot be read is left alone rather than lose its contents.
        let mut doc = match Document::read(&path) {
            Ok(doc) => doc,
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Document::default(),
            Err(e) => return Err(format!("{name} is not readable as a playlist ({e}), so it was left alone")),
        };
        let mut player = doc.player.take().unwrap_or_default();
        let old = player.playlist.take().unwrap_or_default();
        // Keys other tools set on an entry stay with that entry's file.
        let mut extras: Vec<(PathBuf, _)> = old
            .entries
            .into_iter()
            .filter(|e| !e.other.is_empty())
            .map(|e| {
                let file = e.resolve(&folder);
                (std::fs::canonicalize(&file).unwrap_or(file), e.other)
            })
            .collect();
        let entries = self
            .playlist
            .iter()
            .zip(&self.entry_dissolve)
            .map(|(file, dissolve)| Entry {
                path: relative_path(file, &folder),
                dissolve: dissolve.map(file_seconds),
                other: extras.iter().position(|(f, _)| f == file).map(|i| extras.remove(i).1).unwrap_or_default(),
            })
            .collect();
        // Lengths are written per entry, so the playlist-wide one (already
        // given to each of its entries when opened) is not kept.
        player.playlist = Some(Playlist { dissolve: None, entries, other: old.other });
        doc.player = Some(player);
        doc.write(&path).map_err(|e| format!("{name}: {e}"))?;
        Ok(path)
    }
}
