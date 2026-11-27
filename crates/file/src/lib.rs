//! `.airsplice` files: the YAML documents AirSplice tools share.
//!
//! A file names its format version and holds one section per tool, the
//! player's under `player`:
//!
//! ```yaml
//! version: 1
//! player:
//!   playlist:
//!     dissolve: 1        # seconds, for every entry without its own
//!     entries:
//!     - intro.mp4
//!     - path: title.png
//!       dissolve: 2.5    # seconds into this entry; 0 cuts
//! ```
//!
//! `version` is the format version. Adding an optional key never changes
//! it; changing what an existing key means does, and a reader refuses a
//! version newer than its own. Keys a tool does not know, at the top level
//! or inside a section or entry, are kept as they were when it writes the
//! file back, so tools can add their own without breaking each other.
//! Relative paths are relative to the file's folder.

use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde_norway::{Mapping, Value};

/// The file extension, without the dot.
pub const EXTENSION: &str = "airsplice";

/// The newest format version this crate reads, and the one it writes.
pub const VERSION: u64 = 1;

/// Whether `path` has the `.airsplice` extension.
pub fn is_airsplice_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case(EXTENSION))
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Document {
    /// The player's section.
    pub player: Option<Player>,
    /// Top-level sections this crate does not know (other tools'), in the
    /// order they were read.
    pub other: Mapping,
}

/// What airsplice-player keeps in a file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Player {
    pub playlist: Option<Playlist>,
    pub other: Mapping,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Playlist {
    /// Seconds each entry dissolves in over, unless it gives its own.
    pub dissolve: Option<f64>,
    pub entries: Vec<Entry>,
    pub other: Mapping,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// As written: relative to the file's folder, or absolute.
    pub path: PathBuf,
    /// Seconds this entry dissolves in over; 0 cuts to it.
    pub dissolve: Option<f64>,
    pub other: Mapping,
}

impl Entry {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), dissolve: None, other: Mapping::new() }
    }

    /// The entry's path, with a relative one taken from `folder` (the
    /// folder the file is in).
    pub fn resolve(&self, folder: &Path) -> PathBuf {
        folder.join(&self.path)
    }
}

impl Playlist {
    /// Seconds `entry` dissolves in over: its own, else the playlist's.
    pub fn dissolve_into(&self, entry: &Entry) -> Option<f64> {
        entry.dissolve.or(self.dissolve)
    }
}

/// `path` as an entry in a file in `folder`: relative (with `/`, so the
/// file works on every system) when it is inside that folder, else as is.
pub fn relative_path(path: &Path, folder: &Path) -> PathBuf {
    let parts: Option<Vec<&str>> = path.strip_prefix(folder).ok().and_then(|rel| {
        rel.components()
            .map(|c| match c {
                Component::Normal(name) => name.to_str(),
                _ => None,
            })
            .collect()
    });
    match parts {
        Some(parts) if !parts.is_empty() => PathBuf::from(parts.join("/")),
        _ => path.to_path_buf(),
    }
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Yaml(serde_norway::Error),
    /// No `version:` at the top.
    NotAirsplice,
    /// Written for a newer version of the format.
    Newer(u64),
    Invalid(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => e.fmt(f),
            Error::Yaml(e) => write!(f, "not valid YAML: {e}"),
            Error::NotAirsplice => f.write_str("not an airsplice file: it has no version"),
            Error::Newer(v) => write!(f, "written for airsplice format version {v}; this reads up to {VERSION}"),
            Error::Invalid(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Yaml(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_norway::Error> for Error {
    fn from(e: serde_norway::Error) -> Self {
        Error::Yaml(e)
    }
}

const VERSION_KEY: &str = "version";
const PLAYER_KEY: &str = "player";
const PLAYLIST_KEY: &str = "playlist";

impl Document {
    pub fn read(path: &Path) -> Result<Self, Error> {
        Self::from_yaml(&std::fs::read_to_string(path)?)
    }

    /// Write `path` atomically: a crash mid-write never leaves half a file.
    pub fn write(&self, path: &Path) -> Result<(), Error> {
        let yaml = self.to_yaml()?;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, yaml)?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        Ok(())
    }

    pub fn from_yaml(text: &str) -> Result<Self, Error> {
        let Value::Mapping(mut top) = serde_norway::from_str::<Value>(text)? else { return Err(Error::NotAirsplice) };
        // The version first: a newer file is reported as newer, not as
        // whatever its new shape fails on.
        let version = match top.remove(VERSION_KEY) {
            Some(Value::Number(n)) => n.as_u64().filter(|v| *v >= 1).ok_or_else(|| invalid("version: a whole number from 1"))?,
            Some(_) => return Err(invalid("version: a whole number from 1")),
            None => return Err(Error::NotAirsplice),
        };
        if version > VERSION {
            return Err(Error::Newer(version));
        }
        let player = top.remove(PLAYER_KEY).map(parse_player).transpose()?;
        Ok(Self { player, other: top })
    }

    pub fn to_yaml(&self) -> Result<String, Error> {
        let mut top = Mapping::new();
        top.insert(VERSION_KEY.into(), VERSION.into());
        if let Some(p) = &self.player {
            top.insert(PLAYER_KEY.into(), player_value(p)?);
        }
        for (k, v) in &self.other {
            if !matches!(k.as_str(), Some(VERSION_KEY | PLAYER_KEY)) {
                top.insert(k.clone(), v.clone());
            }
        }
        Ok(serde_norway::to_string(&top)?)
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

fn seconds(value: Value, at: &str) -> Result<f64, Error> {
    value.as_f64().filter(|s| s.is_finite() && *s >= 0.0).ok_or_else(|| invalid(format!("{at}: dissolve is a number of seconds, 0 or more")))
}

fn parse_player(value: Value) -> Result<Player, Error> {
    let mut map = match value {
        Value::Mapping(map) => map,
        Value::Null => Mapping::new(),
        _ => return Err(invalid("player: expected a mapping")),
    };
    let playlist = map.remove(PLAYLIST_KEY).map(parse_playlist).transpose()?;
    Ok(Player { playlist, other: map })
}

fn parse_playlist(value: Value) -> Result<Playlist, Error> {
    let Value::Mapping(mut map) = value else { return Err(invalid("player.playlist: expected a mapping with entries")) };
    let dissolve = map.remove("dissolve").map(|v| seconds(v, "player.playlist")).transpose()?;
    let entries = match map.remove("entries") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Sequence(items)) => {
            items.into_iter().enumerate().map(|(i, v)| parse_entry(v, &format!("player.playlist entry {}", i + 1))).collect::<Result<_, _>>()?
        }
        Some(_) => return Err(invalid("player.playlist: entries is a list")),
    };
    Ok(Playlist { dissolve, entries, other: map })
}

fn parse_entry(value: Value, at: &str) -> Result<Entry, Error> {
    match value {
        Value::String(path) if !path.is_empty() => Ok(Entry::new(path)),
        Value::Mapping(mut map) => {
            let path = match map.remove("path") {
                Some(Value::String(p)) if !p.is_empty() => PathBuf::from(p),
                _ => return Err(invalid(format!("{at}: no path"))),
            };
            let dissolve = map.remove("dissolve").map(|v| seconds(v, at)).transpose()?;
            Ok(Entry { path, dissolve, other: map })
        }
        _ => Err(invalid(format!("{at}: expected a path, or a mapping with a path"))),
    }
}

/// Whole seconds without a decimal point, as people write them.
fn seconds_value(s: f64) -> Value {
    if s.fract() == 0.0 && s < 1e15 { (s as u64).into() } else { s.into() }
}

fn path_value(path: &Path) -> Result<Value, Error> {
    path.to_str().map(Value::from).ok_or_else(|| invalid(format!("{} is not valid text", path.display())))
}

fn player_value(p: &Player) -> Result<Value, Error> {
    let mut map = Mapping::new();
    if let Some(list) = &p.playlist {
        map.insert(PLAYLIST_KEY.into(), playlist_value(list)?);
    }
    for (k, v) in &p.other {
        if k.as_str() != Some(PLAYLIST_KEY) {
            map.insert(k.clone(), v.clone());
        }
    }
    Ok(Value::Mapping(map))
}

fn playlist_value(p: &Playlist) -> Result<Value, Error> {
    let mut map = Mapping::new();
    if let Some(d) = p.dissolve {
        map.insert("dissolve".into(), seconds_value(d));
    }
    let entries = p.entries.iter().map(entry_value).collect::<Result<Vec<_>, _>>()?;
    map.insert("entries".into(), Value::Sequence(entries));
    for (k, v) in &p.other {
        if !matches!(k.as_str(), Some("dissolve" | "entries")) {
            map.insert(k.clone(), v.clone());
        }
    }
    Ok(Value::Mapping(map))
}

/// A plain path when that is all there is, so files stay easy to edit.
fn entry_value(e: &Entry) -> Result<Value, Error> {
    if e.dissolve.is_none() && e.other.is_empty() {
        return path_value(&e.path);
    }
    let mut map = Mapping::new();
    map.insert("path".into(), path_value(&e.path)?);
    if let Some(d) = e.dissolve {
        map.insert("dissolve".into(), seconds_value(d));
    }
    for (k, v) in &e.other {
        if !matches!(k.as_str(), Some("path" | "dissolve")) {
            map.insert(k.clone(), v.clone());
        }
    }
    Ok(Value::Mapping(map))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
version: 1
player:
  playlist:
    dissolve: 1
    entries:
    - intro.mp4
    - path: slides/title.png
      dissolve: 2.5
    - path: /media/talk.mp4
      dissolve: 0
";

    #[test]
    fn reads_entries_and_their_dissolves() {
        let doc = Document::from_yaml(SAMPLE).unwrap();
        let p = doc.player.unwrap().playlist.unwrap();
        assert_eq!(p.dissolve, Some(1.0));
        let paths: Vec<_> = p.entries.iter().map(|e| e.path.to_str().unwrap()).collect();
        assert_eq!(paths, ["intro.mp4", "slides/title.png", "/media/talk.mp4"]);
        let into: Vec<_> = p.entries.iter().map(|e| p.dissolve_into(e)).collect();
        assert_eq!(into, [Some(1.0), Some(2.5), Some(0.0)]);
    }

    #[test]
    fn writes_what_it_read() {
        let doc = Document::from_yaml(SAMPLE).unwrap();
        assert_eq!(doc.to_yaml().unwrap(), SAMPLE);
        assert_eq!(Document::from_yaml(&doc.to_yaml().unwrap()).unwrap(), doc);
    }

    #[test]
    fn keeps_what_other_tools_wrote() {
        let text = "\
version: 1
player:
  playlist:
    entries:
    - path: a.mp4
      splice:
        in: 3.5
    loop: true
  volume: 0.5
splice:
  cuts:
  - 1
  - 2
";
        let doc = Document::from_yaml(text).unwrap();
        assert_eq!(doc.to_yaml().unwrap(), text);
    }

    #[test]
    fn refuses_a_newer_format_before_reading_it() {
        // A newer format may change shape; the version is what is reported.
        let err = Document::from_yaml("version: 2\nplayer: [whatever]\n").unwrap_err();
        assert!(matches!(err, Error::Newer(2)), "{err}");
        assert_eq!(err.to_string(), "written for airsplice format version 2; this reads up to 1");
    }

    #[test]
    fn says_what_is_wrong() {
        let cases = [
            ("player:\n  playlist: {}\n", "not an airsplice file: it has no version"),
            ("- a.mp4\n", "not an airsplice file: it has no version"),
            ("version: 0\n", "version: a whole number from 1"),
            ("version: one\n", "version: a whole number from 1"),
            ("version: 1\nplayer: [a.mp4]\n", "player: expected a mapping"),
            ("version: 1\nplayer:\n  playlist:\n    entries:\n    - a.mp4\n    - dissolve: 1\n", "player.playlist entry 2: no path"),
            ("version: 1\nplayer:\n  playlist:\n    entries:\n    - path: a.mp4\n      dissolve: -1\n", "player.playlist entry 1: dissolve is a number of seconds, 0 or more"),
            ("version: 1\nplayer:\n  playlist:\n    dissolve: soon\n", "player.playlist: dissolve is a number of seconds, 0 or more"),
            ("version: 1\nplayer:\n  playlist:\n    entries: a.mp4\n", "player.playlist: entries is a list"),
        ];
        for (text, message) in cases {
            assert_eq!(Document::from_yaml(text).unwrap_err().to_string(), message, "{text}");
        }
    }

    #[test]
    fn a_file_with_only_other_sections_has_no_player_section() {
        let doc = Document::from_yaml("version: 1\nsplice: {}\n").unwrap();
        assert_eq!(doc.player, None);
    }

    #[test]
    fn paths_inside_the_folder_are_written_relative() {
        let folder = Path::new("/shows/spring");
        assert_eq!(relative_path(Path::new("/shows/spring/clips/a.mp4"), folder), PathBuf::from("clips/a.mp4"));
        assert_eq!(relative_path(Path::new("/elsewhere/b.mp4"), folder), PathBuf::from("/elsewhere/b.mp4"));
        let e = Entry::new("clips/a.mp4");
        assert_eq!(e.resolve(folder), PathBuf::from("/shows/spring/clips/a.mp4"));
        assert_eq!(Entry::new("/elsewhere/b.mp4").resolve(folder), PathBuf::from("/elsewhere/b.mp4"));
    }

    #[test]
    fn writes_and_reads_a_file() {
        let dir = std::env::temp_dir().join(format!("airsplice-file-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("show.airsplice");
        let doc = Document::from_yaml(SAMPLE).unwrap();
        doc.write(&path).unwrap();
        assert_eq!(Document::read(&path).unwrap(), doc);
        assert!(is_airsplice_file(&path) && is_airsplice_file(Path::new("A.AIRSPLICE")));
        assert!(!dir.join("show.airsplice.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
