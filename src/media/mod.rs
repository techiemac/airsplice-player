//! Media engine: demuxing, decoding, audio output and A/V sync.

pub mod audio;
pub mod mp4_timing;
pub mod opus;
pub mod picture;
pub mod player;
pub mod upscale;
pub mod video;

pub use player::{Image, MediaInfo, Player, Summary, VideoFrame, probe};

/// File extensions offered in the open dialog and accepted from folders.
pub const MEDIA_EXTENSIONS: &[&str] = &[
    "mp3", "m4a", "m4b", "aac", "flac", "ogg", "oga", "opus", "wav", "wave", "aif", "aiff", "caf", "mka",
    "mp4", "m4v", "mov", "mkv", "webm", "png", "jpg", "jpeg", "gif",
];

pub fn is_media_file(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| MEDIA_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}
