//! Presentation timestamps for MP4/MOV. Symphonia 0.6.1's MP4 reader ignores
//! the composition offsets (`ctts`) and edit lists (`elst`), so the `pts`
//! it reports is really the raw decode time. With B-frames that puts video
//! in decode order, and ignoring the edit list misaligns audio priming.
//! This reads those boxes and maps raw times to presentation times.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Per-track mapping from Symphonia's raw timestamp to presentation time.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TrackTiming {
    /// Added to every timestamp: empty-edit delay minus the first edit's
    /// media time, in the track's timescale.
    pub shift: i64,
    /// Raw decode time to composition offset, for tracks with `ctts`.
    pub composition: HashMap<i64, i64>,
}

impl TrackTiming {
    pub fn pts(&self, raw: i64) -> i64 {
        raw + self.composition.get(&raw).copied().unwrap_or(0) + self.shift
    }
}

/// Timing for every track that needs it, keyed by MP4 track ID (which is
/// the track ID Symphonia reports).
pub fn read(path: &Path) -> HashMap<u32, TrackTiming> {
    read_inner(path).unwrap_or_default()
}

fn read_inner(path: &Path) -> Option<HashMap<u32, TrackTiming>> {
    let moov = find_moov(&mut File::open(path).ok()?)?;
    let movie_timescale = boxes(&moov).find(|(t, _)| t == b"mvhd").and_then(|(_, b)| mvhd_timescale(b))?;
    let mut out = HashMap::new();
    for (_, trak) in boxes(&moov).filter(|(t, _)| t == b"trak") {
        if let Some((id, timing)) = track_timing(trak, movie_timescale)
            && timing != TrackTiming::default()
        {
            out.insert(id, timing);
        }
    }
    Some(out)
}

/// Read the `moov` box, wherever it is in the file, without reading media.
fn find_moov(file: &mut File) -> Option<Vec<u8>> {
    let len = file.seek(SeekFrom::End(0)).ok()?;
    let mut pos = 0u64;
    while pos + 8 <= len {
        file.seek(SeekFrom::Start(pos)).ok()?;
        let mut header = [0u8; 16];
        file.read_exact(&mut header[..8]).ok()?;
        let mut size = u32::from_be_bytes(header[..4].try_into().ok()?) as u64;
        let mut header_len = 8;
        if size == 1 {
            file.read_exact(&mut header[8..16]).ok()?;
            size = u64::from_be_bytes(header[8..16].try_into().ok()?);
            header_len = 16;
        } else if size == 0 {
            size = len - pos;
        }
        if size < header_len {
            return None;
        }
        if &header[4..8] == b"moov" {
            let body = size - header_len;
            if body > 512 * 1024 * 1024 {
                return None;
            }
            let mut buf = vec![0u8; body as usize];
            file.read_exact(&mut buf).ok()?;
            return Some(buf);
        }
        pos += size;
    }
    None
}

/// Iterate the child boxes of a box body.
fn boxes(data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        let header = data.get(pos..pos + 8)?;
        let mut size = u32::from_be_bytes(header[..4].try_into().ok()?) as usize;
        let kind: [u8; 4] = header[4..8].try_into().ok()?;
        let mut header_len = 8;
        if size == 1 {
            size = u64::from_be_bytes(data.get(pos + 8..pos + 16)?.try_into().ok()?) as usize;
            header_len = 16;
        } else if size == 0 {
            size = data.len() - pos;
        }
        let body = data.get(pos + header_len..pos + size)?;
        pos += size;
        Some((kind, body))
    })
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(data).find(|(t, _)| t == kind).map(|(_, b)| b)
}

fn be_u32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

fn be_u64(d: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(d.get(at..at + 8)?.try_into().ok()?))
}

fn mvhd_timescale(b: &[u8]) -> Option<u32> {
    be_u32(b, if b.first()? == &1 { 20 } else { 12 })
}

fn track_timing(trak: &[u8], movie_timescale: u32) -> Option<(u32, TrackTiming)> {
    let tkhd = child(trak, b"tkhd")?;
    let track_id = be_u32(tkhd, if tkhd.first()? == &1 { 20 } else { 12 })?;
    let mdia = child(trak, b"mdia")?;
    let mdhd = child(mdia, b"mdhd")?;
    let media_timescale = be_u32(mdhd, if mdhd.first()? == &1 { 20 } else { 12 })?;
    let stbl = child(child(mdia, b"minf")?, b"stbl")?;

    // Edit list: leading empty edits delay the track (movie timescale);
    // the first real edit's media time is where presentation starts.
    let mut shift = 0i64;
    if let Some(elst) = child(trak, b"edts").and_then(|e| child(e, b"elst")) {
        let v1 = elst.first()? == &1;
        let count = be_u32(elst, 4)? as usize;
        let entry = if v1 { 20 } else { 12 };
        for i in 0..count {
            let at = 8 + i * entry;
            let (duration, media_time) = if v1 {
                (be_u64(elst, at)? as i64, be_u64(elst, at + 8)? as i64)
            } else {
                (be_u32(elst, at)? as i64, be_u32(elst, at + 4)? as i32 as i64)
            };
            if media_time == -1 {
                shift += duration * media_timescale as i64 / movie_timescale.max(1) as i64;
            } else {
                shift -= media_time;
                break;
            }
        }
    }

    let mut composition = HashMap::new();
    if let Some(ctts) = child(stbl, b"ctts") {
        let stts = child(stbl, b"stts")?;
        // Raw decode time of each sample, from the stts run lengths.
        let mut dts = Vec::new();
        let mut t = 0i64;
        for i in 0..be_u32(stts, 4)? as usize {
            let (count, delta) = (be_u32(stts, 8 + i * 8)?, be_u32(stts, 12 + i * 8)? as i64);
            for _ in 0..count {
                dts.push(t);
                t += delta;
            }
        }
        let mut sample = 0usize;
        for i in 0..be_u32(ctts, 4)? as usize {
            let count = be_u32(ctts, 8 + i * 8)?;
            // Signed whatever the version: the spec makes version 0
            // unsigned, but QuickTime writes negative offsets there, and
            // an unsigned offset past 2^31 would mean a frame shown hours
            // after it is decoded. FFmpeg reads them the same way.
            let offset = be_u32(ctts, 12 + i * 8)? as i32 as i64;
            for _ in 0..count {
                if let Some(&d) = dts.get(sample) {
                    composition.insert(d, offset);
                }
                sample += 1;
            }
        }
    }
    Some((track_id, TrackTiming { shift, composition }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    #[test]
    fn b_frame_mp4_maps_to_the_presentation_times_ffprobe_reports() {
        // ffprobe: first video PTS 0, 2048, 1024, 512, 1536 for raw decode
        // times 0, 512, 1024, 1536, 2048 (timescale 12800, 25 fps).
        let timing = read(&fixture("bframes_h264.mp4"));
        let video = timing.get(&1).expect("video track timing");
        let pts: Vec<i64> = [0, 512, 1024, 1536, 2048].iter().map(|&r| video.pts(r)).collect();
        assert_eq!(pts, vec![0, 2048, 1024, 512, 1536]);
    }

    #[test]
    fn negative_offsets_in_a_version_0_ctts_are_signed() {
        // QuickTime stores negative composition offsets in version 0 boxes.
        // ffprobe: PTS 0, 1536, 512, 1024, 3072 for raw decode times 0, 512,
        // 1024, 1536, 2048. Read as unsigned, B-frames land 2^32 ticks
        // (about two days) in the future and are never shown.
        let timing = read(&fixture("ctts_v0_negative.mov"));
        let video = timing.get(&1).expect("video track timing");
        let pts: Vec<i64> = [0, 512, 1024, 1536, 2048].iter().map(|&r| video.pts(r)).collect();
        assert_eq!(pts, vec![0, 1536, 512, 1024, 3072]);
    }

    #[test]
    fn aac_priming_comes_from_the_edit_list() {
        // ffprobe: first AAC packet PTS -1024 at raw 0.
        let timing = read(&fixture("h264_aac.mp4"));
        let audio = timing.values().find(|t| t.composition.is_empty() && t.shift == -1024);
        assert!(audio.is_some(), "{timing:?}");
    }

    #[test]
    fn non_mp4_and_missing_files_yield_nothing() {
        assert!(read(&fixture("av1_opus.webm")).is_empty());
        assert!(read(&fixture("does-not-exist.mp4")).is_empty());
    }
}
