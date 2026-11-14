//! Opus decoding with `ropus`, a pure-Rust port of libopus that is bit-exact
//! against the C reference. Symphonia demuxes Opus (Ogg, Matroska/WebM) but
//! ships no Opus decoder.
//!
//! (`opus-decoder` 0.1.1 was tried first and rejected: its multistream
//! framing does not follow RFC 6716 Appendix B, and its CELT collapse mask
//! overflows a `u8` on real hybrid stereo frames.)

use anyhow::{Result, anyhow, bail};
use ropus::OpusMSDecoder;

/// Opus always decodes at 48 kHz.
pub const OPUS_RATE: u32 = 48_000;
/// 120 ms, the longest Opus packet.
const MAX_FRAME: usize = 5760;

pub struct Opus {
    decoder: OpusMSDecoder,
    channels: usize,
    /// Maps output index (Symphonia order) to decoded index (Vorbis order).
    reorder: Option<Vec<usize>>,
    pcm: Vec<i16>,
}

#[derive(Debug, PartialEq)]
pub struct OpusHead {
    pub channels: usize,
    pub family: u8,
    pub streams: usize,
    pub coupled: usize,
    pub mapping: Vec<u8>,
}

impl OpusHead {
    fn simple(channels: usize) -> Self {
        let channels = channels.clamp(1, 2);
        OpusHead {
            channels,
            family: 0,
            streams: 1,
            coupled: usize::from(channels == 2),
            mapping: (0..channels as u8).collect(),
        }
    }
}

/// Parse an `OpusHead` identification header (RFC 7845 section 5.1).
pub fn parse_opus_head(data: &[u8]) -> Result<OpusHead> {
    if data.len() < 19 || &data[..8] != b"OpusHead" {
        bail!("missing OpusHead");
    }
    let channels = data[9] as usize;
    let family = data[18];
    if channels == 0 {
        bail!("OpusHead declares zero channels");
    }
    if family == 0 {
        if channels > 2 {
            bail!("Opus mapping family 0 with {channels} channels");
        }
        return Ok(OpusHead::simple(channels));
    }
    let table = data.get(19..21 + channels).ok_or_else(|| anyhow!("truncated OpusHead mapping table"))?;
    Ok(OpusHead { channels, family, streams: table[0] as usize, coupled: table[1] as usize, mapping: table[2..].to_vec() })
}

/// Vorbis channel order (used by Opus mapping family 1) to Symphonia's
/// position-bit order (FL, FR, FC, LFE, RL, RR, SL, SR).
fn vorbis_to_symphonia(channels: usize) -> Option<Vec<usize>> {
    Some(match channels {
        3 => vec![0, 2, 1],
        5 => vec![0, 2, 1, 3, 4],
        6 => vec![0, 2, 1, 5, 3, 4],
        7 => vec![0, 2, 1, 6, 5, 3, 4],
        8 => vec![0, 2, 1, 7, 5, 6, 3, 4],
        _ => return None,
    })
}

impl Opus {
    pub fn new(extra_data: Option<&[u8]>, channels_hint: usize) -> Result<Self> {
        let head = match extra_data {
            Some(data) => parse_opus_head(data)?,
            None => OpusHead::simple(channels_hint),
        };
        let decoder = OpusMSDecoder::new(
            OPUS_RATE as i32,
            head.channels as i32,
            head.streams as i32,
            head.coupled as i32,
            &head.mapping,
        )
        .map_err(|code| anyhow!("opus: invalid stream layout (error {code})"))?;
        let reorder = if head.family == 1 { vorbis_to_symphonia(head.channels) } else { None };
        Ok(Self { decoder, channels: head.channels, reorder, pcm: vec![0; MAX_FRAME * head.channels] })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Decode a packet into interleaved f32, replacing `out`'s contents.
    pub fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<usize> {
        let frames = self
            .decoder
            .decode(Some(packet), packet.len() as i32, &mut self.pcm, MAX_FRAME as i32, false)
            .map_err(|code| anyhow!("opus: decode error {code}"))? as usize;
        let ch = self.channels;
        let scale = 1.0 / 32768.0;
        out.clear();
        out.reserve(frames * ch);
        match &self.reorder {
            None => out.extend(self.pcm[..frames * ch].iter().map(|&s| s as f32 * scale)),
            Some(map) => {
                for f in self.pcm[..frames * ch].chunks_exact(ch) {
                    out.extend(map.iter().map(|&i| f[i] as f32 * scale));
                }
            }
        }
        Ok(frames)
    }

    pub fn reset(&mut self) {
        self.decoder.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stereo_and_surround_heads() {
        let mut stereo = b"OpusHead".to_vec();
        stereo.extend_from_slice(&[1, 2, 0x38, 0x01, 0x80, 0xbb, 0, 0, 0, 0, 0]);
        let h = parse_opus_head(&stereo).unwrap();
        assert_eq!((h.channels, h.family, h.streams, h.coupled), (2, 0, 1, 1));

        let mut surround = b"OpusHead".to_vec();
        surround.extend_from_slice(&[1, 6, 0x38, 0x01, 0x80, 0xbb, 0, 0, 0, 0, 1, 4, 2, 0, 4, 1, 2, 3, 5]);
        let h = parse_opus_head(&surround).unwrap();
        assert_eq!((h.channels, h.family, h.streams, h.coupled), (6, 1, 4, 2));
        assert_eq!(h.mapping, vec![0, 4, 1, 2, 3, 5]);

        assert!(parse_opus_head(&surround[..20]).is_err(), "truncated mapping table");
    }

    #[test]
    fn surround_reorder_puts_centre_third_and_lfe_fourth() {
        // Vorbis order: L C R RL RR LFE
        let vorbis = ["L", "C", "R", "RL", "RR", "LFE"];
        let map = vorbis_to_symphonia(6).unwrap();
        let out: Vec<_> = map.iter().map(|&i| vorbis[i]).collect();
        assert_eq!(out, ["L", "R", "C", "LFE", "RL", "RR"]);
    }
}
