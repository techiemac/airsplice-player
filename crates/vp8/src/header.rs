//! The frame header (RFC 6386 section 9 and 19.2) and the state it updates
//! that outlives a frame.

use crate::Error;
use crate::bool_decoder::BoolDecoder;
use crate::tables::*;

/// The entropy probabilities a frame may update. When a frame clears
/// `refresh_entropy_probs` its updates apply to that frame only.
#[derive(Clone)]
pub struct Probs {
    /// Block type, band, context, token tree node.
    pub coeff: [[[[u8; 11]; 3]; 8]; 4],
    pub ymode: [u8; 4],
    pub uv_mode: [u8; 3],
    /// Row then column.
    pub mv: [[u8; MV_PROB_COUNT]; 2],
}

impl Probs {
    fn defaults() -> Probs {
        Probs {
            coeff: DEFAULT_COEFF_PROBS,
            ymode: DEFAULT_YMODE_PROBS,
            uv_mode: DEFAULT_UV_MODE_PROBS,
            mv: DEFAULT_MV_PROBS,
        }
    }
}

/// Everything in the header that persists from frame to frame until a key
/// frame resets it. (The per-macroblock segment map persists too; it lives
/// with the macroblock info.)
#[derive(Clone)]
pub struct Persistent {
    pub probs: Probs,
    /// Per-segment quantizer index and loop filter level, absolute or deltas.
    pub segment_abs: bool,
    pub segment_quant: [i8; 4],
    pub segment_lf: [i8; 4],
    /// Loop filter level adjustments by reference frame and by mode class.
    pub ref_lf_deltas: [i8; 4],
    pub mode_lf_deltas: [i8; 4],
}

impl Persistent {
    pub fn new() -> Persistent {
        Persistent {
            probs: Probs::defaults(),
            segment_abs: false,
            segment_quant: [0; 4],
            segment_lf: [0; 4],
            ref_lf_deltas: [0; 4],
            mode_lf_deltas: [0; 4],
        }
    }
}

/// The three bytes (ten on key frames) before the first partition.
pub struct FrameTag {
    pub key_frame: bool,
    pub version: u8,
    pub show_frame: bool,
    pub first_part_len: usize,
    /// Key frames only.
    pub width: u32,
    pub height: u32,
    /// Bytes the tag occupies.
    pub len: usize,
}

impl FrameTag {
    pub fn parse(data: &[u8]) -> Result<FrameTag, Error> {
        let [b0, b1, b2, ..] = *data else {
            return Err(Error::Corrupt("truncated frame tag"));
        };
        let bits = u32::from(b0) | u32::from(b1) << 8 | u32::from(b2) << 16;
        let mut tag = FrameTag {
            key_frame: bits & 1 == 0,
            version: ((bits >> 1) & 7) as u8,
            show_frame: (bits >> 4) & 1 == 1,
            first_part_len: (bits >> 5) as usize,
            width: 0,
            height: 0,
            len: 3,
        };
        if tag.first_part_len == 0 {
            return Err(Error::Corrupt("empty first partition"));
        }
        if tag.key_frame {
            let Some(&[s0, s1, s2, w0, w1, h0, h1]) = data[3..].first_chunk::<7>() else {
                return Err(Error::Corrupt("truncated key frame header"));
            };
            if [s0, s1, s2] != [0x9d, 0x01, 0x2a] {
                return Err(Error::Corrupt("bad key frame start code"));
            }
            // The top two bits of each dimension are an upscaling hint for
            // the display. Like libvpx, we parse past them and ignore them:
            // the decoded picture is always the coded size.
            tag.width = u32::from(u16::from_le_bytes([w0, w1]) & 0x3fff);
            tag.height = u32::from(u16::from_le_bytes([h0, h1]) & 0x3fff);
            tag.len = 10;
            if tag.width == 0 || tag.height == 0 {
                return Err(Error::Corrupt("zero frame size"));
            }
        }
        if data.len() - tag.len < tag.first_part_len {
            return Err(Error::Corrupt("truncated first partition"));
        }
        Ok(tag)
    }
}

/// How a frame updates the golden or altref buffer when it does not replace
/// it with itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CopyRef {
    None,
    Last,
    /// The other one of golden and altref.
    Other,
}

/// The per-frame fields of the header.
pub struct FrameHeader {
    pub key_frame: bool,
    pub segmentation: bool,
    pub update_segment_map: bool,
    pub segment_tree_probs: [u8; 3],
    /// The header changed segment data or loop filter deltas, which later
    /// frames inherit.
    pub updates_state: bool,
    /// The header changed an entropy probability.
    pub updates_probs: bool,
    pub simple_filter: bool,
    pub filter_level: u8,
    pub sharpness: u8,
    pub lf_deltas: bool,
    pub log2_partitions: u32,
    pub base_q: i32,
    /// Quantizer index deltas: Y1 DC, Y2 DC, Y2 AC, UV DC, UV AC.
    pub q_deltas: [i32; 5],
    pub refresh_golden: bool,
    pub refresh_altref: bool,
    pub copy_to_golden: CopyRef,
    pub copy_to_altref: CopyRef,
    /// Indexed by reference frame (intra, last, golden, altref).
    pub sign_bias: [bool; 4],
    pub refresh_probs: bool,
    pub refresh_last: bool,
    /// Zero when macroblocks carry no skip flag.
    pub skip_enabled: bool,
    pub prob_skip_false: u8,
    pub prob_intra: u8,
    pub prob_last: u8,
    pub prob_golden: u8,
}

fn read_flagged_signed(bd: &mut BoolDecoder, bits: u32) -> Option<i8> {
    bd.read_bit().then(|| bd.read_signed(bits) as i8)
}

fn read_copy(bd: &mut BoolDecoder) -> Result<CopyRef, Error> {
    match bd.read_literal(2) {
        0 => Ok(CopyRef::None),
        1 => Ok(CopyRef::Last),
        2 => Ok(CopyRef::Other),
        _ => Err(Error::Corrupt("invalid reference copy flag")),
    }
}

impl FrameHeader {
    /// Parse the header from the first partition, applying its updates to
    /// `state`. Returns the header and, when the frame's probability updates
    /// are for this frame only, the probabilities to restore afterwards.
    pub fn parse(
        bd: &mut BoolDecoder,
        key_frame: bool,
        state: &mut Persistent,
    ) -> Result<(FrameHeader, Option<Probs>), Error> {
        if key_frame {
            *state = Persistent::new();
            // Color space and clamping type: libvpx reads and ignores both
            // (reconstruction always clamps).
            bd.read_bit();
            bd.read_bit();
        }

        let segmentation = bd.read_bit();
        let mut update_segment_map = false;
        let mut segment_tree_probs = [255; 3];
        let mut updates_state = false;
        if segmentation {
            update_segment_map = bd.read_bit();
            if bd.read_bit() {
                updates_state = true;
                state.segment_abs = bd.read_bit();
                for q in &mut state.segment_quant {
                    *q = read_flagged_signed(bd, 7).unwrap_or(0);
                }
                for l in &mut state.segment_lf {
                    *l = read_flagged_signed(bd, 6).unwrap_or(0);
                }
            }
            if update_segment_map {
                for p in &mut segment_tree_probs {
                    if bd.read_bit() {
                        *p = bd.read_literal(8) as u8;
                    }
                }
            }
        }

        let simple_filter = bd.read_bit();
        let filter_level = bd.read_literal(6) as u8;
        let sharpness = bd.read_literal(3) as u8;
        let lf_deltas = bd.read_bit();
        if lf_deltas && bd.read_bit() {
            updates_state = true;
            for d in state.ref_lf_deltas.iter_mut().chain(&mut state.mode_lf_deltas) {
                if let Some(v) = read_flagged_signed(bd, 6) {
                    *d = v;
                }
            }
        }

        let log2_partitions = bd.read_literal(2);

        let base_q = bd.read_literal(7) as i32;
        let mut q_deltas = [0; 5];
        for d in &mut q_deltas {
            if bd.read_bit() {
                *d = bd.read_signed(4);
            }
        }

        let mut refresh_golden = true;
        let mut refresh_altref = true;
        let mut copy_to_golden = CopyRef::None;
        let mut copy_to_altref = CopyRef::None;
        let mut sign_bias = [false; 4];
        if !key_frame {
            refresh_golden = bd.read_bit();
            refresh_altref = bd.read_bit();
            if !refresh_golden {
                copy_to_golden = read_copy(bd)?;
            }
            if !refresh_altref {
                copy_to_altref = read_copy(bd)?;
            }
            sign_bias[2] = bd.read_bit();
            sign_bias[3] = bd.read_bit();
        }

        let refresh_probs = bd.read_bit();
        let saved = (!refresh_probs).then(|| state.probs.clone());
        let refresh_last = key_frame || bd.read_bit();

        let probs = &mut state.probs;
        let mut updates_probs = false;
        for (p, update) in probs.coeff.as_flattened_mut().as_flattened_mut().as_flattened_mut().iter_mut().zip(
            COEFF_UPDATE_PROBS.as_flattened().as_flattened().as_flattened(),
        ) {
            if bd.read(*update) {
                updates_probs = true;
                *p = bd.read_literal(8) as u8;
            }
        }

        let skip_enabled = bd.read_bit();
        let prob_skip_false = if skip_enabled { bd.read_literal(8) as u8 } else { 0 };

        let (mut prob_intra, mut prob_last, mut prob_golden) = (0, 0, 0);
        if !key_frame {
            prob_intra = bd.read_literal(8) as u8;
            prob_last = bd.read_literal(8) as u8;
            prob_golden = bd.read_literal(8) as u8;
            if bd.read_bit() {
                updates_probs = true;
                for p in &mut probs.ymode {
                    *p = bd.read_literal(8) as u8;
                }
            }
            if bd.read_bit() {
                updates_probs = true;
                for p in &mut probs.uv_mode {
                    *p = bd.read_literal(8) as u8;
                }
            }
            for (p, update) in probs.mv.as_flattened_mut().iter_mut().zip(MV_UPDATE_PROBS.as_flattened()) {
                if bd.read(*update) {
                    updates_probs = true;
                    let x = bd.read_literal(7) as u8;
                    *p = if x == 0 { 1 } else { x << 1 };
                }
            }
        }

        let header = FrameHeader {
            key_frame,
            segmentation,
            update_segment_map,
            segment_tree_probs,
            updates_state,
            updates_probs,
            simple_filter,
            filter_level,
            sharpness,
            lf_deltas,
            log2_partitions,
            base_q,
            q_deltas,
            refresh_golden,
            refresh_altref,
            copy_to_golden,
            copy_to_altref,
            sign_bias,
            refresh_probs,
            refresh_last,
            skip_enabled,
            prob_skip_false,
            prob_intra,
            prob_last,
            prob_golden,
        };
        Ok((header, saved))
    }
}

/// Split the bytes after the first partition into the token partitions.
/// The sizes of all but the last are stored up front, three bytes each.
pub fn token_partitions(data: &[u8], log2_count: u32) -> Result<[&[u8]; 8], Error> {
    let count = 1usize << log2_count;
    let Some((sizes, mut body)) = data.split_at_checked(3 * (count - 1)) else {
        return Err(Error::Corrupt("truncated partition sizes"));
    };
    let mut parts: [&[u8]; 8] = [&[]; 8];
    for (part, size) in parts.iter_mut().zip(sizes.as_chunks::<3>().0) {
        // Once the data is used up the remaining partitions are empty,
        // whatever their stored sizes say (libvpx stops looking too).
        if body.is_empty() {
            return Ok(parts);
        }
        let size = usize::from(size[0]) | usize::from(size[1]) << 8 | usize::from(size[2]) << 16;
        let Some((this, rest)) = body.split_at_checked(size) else {
            return Err(Error::Corrupt("truncated token partition"));
        };
        *part = this;
        body = rest;
    }
    parts[count - 1] = body;
    Ok(parts)
}

/// Dequantization factors for one segment.
#[derive(Clone, Copy, Default)]
pub struct Dequant {
    /// DC then AC factor for each plane type.
    pub y1: [i16; 2],
    pub y2: [i16; 2],
    pub uv: [i16; 2],
}

impl Dequant {
    /// The factors for each of the four segments (all equal when
    /// segmentation is off).
    pub fn for_frame(header: &FrameHeader, state: &Persistent) -> [Dequant; 4] {
        let lookup = |table: &[u16; 128], q: i32| i32::from(table[q.clamp(0, 127) as usize]);
        let [y1dc, y2dc, y2ac, uvdc, uvac] = header.q_deltas;
        std::array::from_fn(|segment| {
            let q = if !header.segmentation {
                header.base_q
            } else if state.segment_abs {
                i32::from(state.segment_quant[segment])
            } else {
                header.base_q + i32::from(state.segment_quant[segment])
            };
            let q = q.clamp(0, 127);
            Dequant {
                y1: [lookup(&DC_Q, q + y1dc) as i16, lookup(&AC_Q, q) as i16],
                y2: [
                    (lookup(&DC_Q, q + y2dc) * 2) as i16,
                    (lookup(&AC_Q, q + y2ac) * 155 / 100).max(8) as i16,
                ],
                uv: [lookup(&DC_Q, q + uvdc).min(132) as i16, lookup(&AC_Q, q + uvac) as i16],
            }
        })
    }
}
