//! A pure-Rust VP8 decoder, bit-exact with libvpx (RFC 6386).
//!
//! Feed it one compressed frame at a time, as stored in WebM or IVF, and it
//! returns the shown frames as 8-bit 4:2:0 planes.
//!
//! Layout: `header` parses the frame header, `modes` the per-macroblock
//! prediction modes and motion vectors, `tokens` the coefficients; `intra`
//! and `inter` build predictions, `loopfilter` drives the deblocking filter
//! and `dsp` holds the pixel kernels. `decoder` ties them together.

mod bool_decoder;
mod decoder;
pub mod dsp;
mod frame;
mod header;
mod inter;
mod intra;
mod loopfilter;
mod modes;
mod tables;
mod tokens;

use std::fmt;

pub use decoder::Decoder;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The data ends before the frame does, or a header field is invalid.
    Corrupt(&'static str),
    /// An inter frame arrived before any keyframe.
    NoKeyframe,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Corrupt(why) => write!(f, "corrupt VP8 frame: {why}"),
            Error::NoKeyframe => write!(f, "VP8 stream does not start with a keyframe"),
        }
    }
}

impl std::error::Error for Error {}

/// A decoded picture. The planes are cropped views of the decoder's buffers:
/// rows are `y_stride` / `uv_stride` bytes apart and only the first `width`
/// (or `(width + 1) / 2` for chroma) bytes of each row are picture.
///
/// `width` and `height` are at least 1. `y` holds at least
/// `(height - 1) * y_stride + width` bytes, and `u` and `v` at least
/// `((height + 1) / 2 - 1) * uv_stride + (width + 1) / 2`.
pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub y_stride: usize,
    pub uv_stride: usize,
}

/// Whether a compressed frame is a keyframe (decoding can start there).
pub fn is_keyframe(data: &[u8]) -> bool {
    data.len() >= 10 && data[0] & 1 == 0 && data[3..6] == [0x9d, 0x01, 0x2a]
}

/// Whether a compressed frame can be skipped without affecting any later
/// frame, so a player running late may drop it undecoded.
///
/// True for an inter frame that leaves every reference alone (it refreshes
/// none of last, golden and altref, and copies none into another) and also
/// changes nothing else later frames inherit: entropy probabilities, the
/// segment map and data, and loop filter deltas. False for keyframes and for
/// anything that does not parse.
pub fn is_droppable(data: &[u8]) -> bool {
    let Ok(tag) = header::FrameTag::parse(data) else {
        return false;
    };
    if tag.key_frame {
        return false;
    }
    let mut bd = bool_decoder::BoolDecoder::new(&data[tag.len..]);
    let mut scratch = header::Persistent::new();
    let Ok((h, _)) = header::FrameHeader::parse(&mut bd, false, &mut scratch) else {
        return false;
    };
    let keeps_refs = !h.refresh_last
        && !h.refresh_golden
        && !h.refresh_altref
        && h.copy_to_golden == header::CopyRef::None
        && h.copy_to_altref == header::CopyRef::None;
    let keeps_probs = !h.refresh_probs || !h.updates_probs;
    keeps_refs && keeps_probs && !h.updates_state && !h.update_segment_map
}
