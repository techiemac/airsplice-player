//! Folders added to the playlist stay in step with the disk: files added to
//! them later are picked up, files deleted from them drop out.
//!
//! The OS reports changes (FSEvents, inotify, ReadDirectoryChangesW via
//! `notify`); once a folder has been quiet for a moment it is rescanned on a
//! worker thread and the result is collected by [`Folders::poll`]. Watching
//! only says *that* something changed; the rescan says *what*, so missed or
//! coalesced events cannot leave the playlist wrong for long.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::media::is_media_file;

/// How long a folder must be quiet before it is rescanned, so a burst of
/// events (a copy, an unzip) costs one scan.
const QUIET: Duration = Duration::from_millis(300);

/// A file modified more recently than this may still be being written (a
/// copy or download in progress); it is left out and looked at again later,
/// so it is never probed half written.
pub const SETTLE: Duration = Duration::from_secs(2);

/// Size and modification time: when either changes, a file's tags and
/// duration are read again.
pub type Stamp = (u64, Option<SystemTime>);

/// The media files found under one folder, sorted, with their stamps.
/// `None` when the folder is missing or unreadable (an unmounted drive):
/// its playlist entries are then left alone rather than dropped.
pub type Listing = Option<Vec<(PathBuf, Stamp)>>;

pub struct Scan {
    pub folders: Vec<(PathBuf, Listing)>,
}

#[derive(Default)]
pub struct Folders {
    roots: Vec<PathBuf>,
    watcher: Option<RecommendedWatcher>,
    /// When the watcher last saw a change, written from its thread.
    changed: Arc<Mutex<Option<Instant>>>,
    /// When to rescan regardless of events (at launch, or to look again at
    /// files that were still being written).
    due: Option<Instant>,
    scanning: Option<Receiver<Scan>>,
}

impl Folders {
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Whether `path` is inside one of the folders.
    pub fn contains(&self, path: &Path) -> bool {
        self.roots.iter().any(|r| path.starts_with(r))
    }

    /// Start following `dir`. A folder inside one already followed adds
    /// nothing; followed folders inside `dir` are folded into it.
    pub fn add(&mut self, dir: PathBuf) {
        if self.contains(&dir) {
            return;
        }
        self.roots.retain(|r| !r.starts_with(&dir));
        self.roots.push(dir);
        self.rewatch();
    }

    /// Follow exactly `roots` (a restored session) and rescan them now, for
    /// files added while the app was closed.
    pub fn set(&mut self, roots: Vec<PathBuf>) {
        self.roots.clear();
        for r in roots {
            if !self.contains(&r) {
                self.roots.retain(|o| !o.starts_with(&r));
                self.roots.push(r);
            }
        }
        self.rewatch();
        self.due = (!self.roots.is_empty()).then(Instant::now);
    }

    pub fn clear(&mut self) {
        self.roots.clear();
        self.watcher = None;
        self.due = None;
        self.scanning = None;
        *self.changed.lock().unwrap() = None;
    }

    /// Rescan after `delay` even if nothing reports a change.
    pub fn rescan_after(&mut self, delay: Duration) {
        let at = Instant::now() + delay;
        self.due = Some(self.due.map_or(at, |d| d.min(at)));
    }

    /// A finished rescan, if one is ready; starts one when a change has
    /// settled or a rescan is due. Never blocks.
    pub fn poll(&mut self) -> Option<Scan> {
        if let Some(rx) = &self.scanning {
            return match rx.try_recv() {
                Ok(scan) => {
                    self.scanning = None;
                    if scan.folders.iter().any(|(_, l)| l.as_ref().is_some_and(|files| files.iter().any(|(_, s)| !settled(s)))) {
                        self.rescan_after(SETTLE);
                    }
                    Some(scan)
                }
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    self.scanning = None;
                    None
                }
            };
        }
        let now = Instant::now();
        {
            let mut changed = self.changed.lock().unwrap();
            if changed.is_some_and(|at| now.duration_since(at) >= QUIET) {
                *changed = None;
                self.due = Some(now);
            }
        }
        if self.due.is_none_or(|d| d > now) || self.roots.is_empty() {
            return None;
        }
        self.due = None;
        let roots = self.roots.clone();
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new().name("folder-scan".into()).spawn(move || {
            let folders = roots.into_iter().map(|r| { let l = list(&r); (r, l) }).collect();
            let _ = tx.send(Scan { folders });
        });
        match spawned {
            Ok(_) => self.scanning = Some(rx),
            Err(e) => log::warn!("could not start a folder scan: {e}"),
        }
        None
    }

    fn rewatch(&mut self) {
        self.watcher = None;
        if self.roots.is_empty() {
            return;
        }
        let changed = self.changed.clone();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
            // Reads (our own playback and probing among them) change nothing.
            Ok(e) if matches!(e.kind, EventKind::Access(_)) => {}
            Ok(_) => *changed.lock().unwrap() = Some(Instant::now()),
            Err(e) => {
                log::warn!("folder watch: {e}");
                *changed.lock().unwrap() = Some(Instant::now());
            }
        });
        let mut watcher = match watcher {
            Ok(w) => w,
            Err(e) => {
                log::warn!("cannot watch folders for new files: {e}");
                return;
            }
        };
        for root in &self.roots {
            if let Err(e) = watcher.watch(root, RecursiveMode::Recursive) {
                log::warn!("cannot watch {} for new files: {e}", root.display());
            }
        }
        self.watcher = Some(watcher);
    }
}

/// Whether a file has gone unmodified for [`SETTLE`]. A modification time
/// in the future (clock skew, a camera with the wrong date) counts as
/// settled, or the file would never appear.
pub fn settled(stamp: &Stamp) -> bool {
    settled_at(stamp, SystemTime::now())
}

fn settled_at(stamp: &Stamp, now: SystemTime) -> bool {
    match stamp.1 {
        Some(modified) => now.duration_since(modified).map_or(true, |age| age >= SETTLE),
        None => true,
    }
}

/// Every media file under `dir`, recursively, sorted by path.
pub fn list(dir: &Path) -> Listing {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, Stamp)>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)?.flatten() {
            let path = entry.path();
            // `metadata` follows symlinks, so a linked file or folder counts.
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            if meta.is_dir() {
                // A subfolder that cannot be read is skipped, not fatal.
                let _ = walk(&path, out);
            } else if is_media_file(&path) {
                out.push((path, (meta.len(), meta.modified().ok())));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, &mut out).ok()?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_counts_as_written_once_it_has_been_left_alone() {
        let now = SystemTime::now();
        assert!(!settled_at(&(1, Some(now)), now), "just written");
        assert!(!settled_at(&(1, Some(now - SETTLE / 2)), now));
        assert!(settled_at(&(1, Some(now - SETTLE)), now));
        assert!(settled_at(&(1, Some(now + Duration::from_secs(3600))), now), "a date in the future does not hide the file");
        assert!(settled_at(&(1, None), now), "no modification time on this filesystem");
    }

    #[test]
    fn nested_folders_are_followed_once() {
        let mut f = Folders::default();
        let base = std::env::temp_dir();
        f.add(base.join("a/b"));
        f.add(base.join("a/b/c"));
        assert_eq!(f.roots(), [base.join("a/b")], "inside a followed folder: nothing new");
        f.add(base.join("a"));
        assert_eq!(f.roots(), [base.join("a")], "the parent takes over");
        assert!(f.contains(&base.join("a/x.mp4")) && !f.contains(&base.join("ab/x.mp4")));
        f.set(vec![base.join("x/y"), base.join("x")]);
        assert_eq!(f.roots(), [base.join("x")]);
    }

    #[test]
    fn listing_is_recursive_sorted_and_media_only() {
        let dir = std::env::temp_dir().join(format!("airsplice-player-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        for name in ["b.mp4", "a.MKV", "notes.txt", "sub/c.mp3"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let names: Vec<_> = list(&dir).unwrap().into_iter().map(|(p, _)| p.strip_prefix(&dir).unwrap().to_path_buf()).collect();
        assert_eq!(names, [PathBuf::from("a.MKV"), "b.mp4".into(), "sub/c.mp3".into()]);
        assert!(list(&dir.join("missing")).is_none(), "a missing folder is not an empty one");
        let _ = std::fs::remove_dir_all(dir);
    }
}
