//! The decoder: reference frame management and the per-frame loop that
//! parses, predicts, adds residuals and filters.

use crate::bool_decoder::BoolDecoder;
use crate::dsp;
use crate::frame::FrameBuf;
use crate::header::{CopyRef, Dequant, FrameHeader, FrameTag, Persistent, token_partitions};
use crate::inter::{self, Interp};
use crate::intra;
use crate::loopfilter::LoopFilter;
use crate::modes::{INTRA_FRAME, MbInfo, ModeParser, MvBounds};
use crate::tables::B_PRED;
use crate::tokens::{self, Coeffs, EdgeContext, Y2};
use crate::{Error, Frame};

/// Last, golden, altref, the frame being decoded, and the last shown frame
/// when no reference holds it any more.
const MAX_BUFFERS: usize = 5;

pub struct Decoder {
    state: Persistent,
    width: u32,
    height: u32,
    mb_cols: usize,
    mb_rows: usize,
    /// Buffer pool; the fields below index it.
    frames: Vec<FrameBuf>,
    last: usize,
    golden: usize,
    altref: usize,
    shown: Option<usize>,
    have_keyframe: bool,
    /// Macroblock info with a border column before each row and a border
    /// row on top (see `ModeParser`). Segment ids persist between frames.
    mbs: Vec<MbInfo>,
    above_ctx: Vec<EdgeContext>,
    /// The bottom pixel row of the macroblock row above, before loop
    /// filtering (intra prediction uses unfiltered neighbours), per plane.
    /// Luma has 4 extra pixels for the last macroblock's above-right.
    above: [Vec<u8>; 3],
    coeffs: Box<Coeffs>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Decoder {
            state: Persistent::new(),
            width: 0,
            height: 0,
            mb_cols: 0,
            mb_rows: 0,
            frames: Vec::new(),
            last: 0,
            golden: 0,
            altref: 0,
            shown: None,
            have_keyframe: false,
            mbs: Vec::new(),
            above_ctx: Vec::new(),
            above: Default::default(),
            coeffs: Box::new(Coeffs::new()),
        }
    }

    /// Decode one compressed frame. Returns the frame to show, or `None` when
    /// the frame is not meant to be shown (`show_frame` is 0). An empty
    /// `data` is a dropped frame: the last shown frame is returned again.
    ///
    /// Until a keyframe has been decoded (at the start and after `reset`),
    /// inter frames and empty `data` return `Err(NoKeyframe)`.
    ///
    /// Every error is detected before the frame changes anything: after an
    /// `Err` the references, probabilities and the last shown frame are
    /// exactly as before the call, so decoding may simply continue with the
    /// next frame (later inter frames then predict from what the failed
    /// frame should have updated, until the next keyframe). Errors are a
    /// truncated or malformed header or partition table, and a keyframe with
    /// zero width or height or a size its data could not possibly fill.
    /// Damage inside the partitions is not detected: it decodes to some
    /// picture.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<Frame<'_>>, Error> {
        if !self.have_keyframe && data.first().is_none_or(|b| b & 1 != 0) {
            return Err(Error::NoKeyframe);
        }
        if data.is_empty() {
            return Ok(self.shown.map(|i| self.frame(i)));
        }
        let tag = FrameTag::parse(data)?;
        let body = &data[tag.len..];
        // As in libvpx, the first partition's decoder is given everything up
        // to the end of the frame, not just the partition's stated length.
        let mut bd = BoolDecoder::new(body);
        let mut state = self.state.clone();
        let (header, saved_probs) = FrameHeader::parse(&mut bd, tag.key_frame, &mut state)?;
        let parts = token_partitions(&body[tag.first_part_len..], header.log2_partitions)?;
        if tag.key_frame && (tag.width, tag.height) != (self.width, self.height) {
            self.resize(&tag)?;
        }
        // Nothing fails from here on.

        let target = self.free_buffer();
        let mut cur = std::mem::take(&mut self.frames[target]);
        if !cur.is_allocated() {
            cur = FrameBuf::new(self.mb_cols, self.mb_rows);
        }
        self.decode_frame(&tag, &header, &state, bd, parts, &mut cur);
        self.frames[target] = cur;

        // Golden and altref copies happen before the refreshes, altref's
        // first: that is libvpx's order, and it matters when both copy.
        match header.copy_to_altref {
            CopyRef::None => {}
            CopyRef::Last => self.altref = self.last,
            CopyRef::Other => self.altref = self.golden,
        }
        match header.copy_to_golden {
            CopyRef::None => {}
            CopyRef::Last => self.golden = self.last,
            CopyRef::Other => self.golden = self.altref,
        }
        if header.refresh_golden {
            self.golden = target;
        }
        if header.refresh_altref {
            self.altref = target;
        }
        if header.refresh_last {
            self.last = target;
        }
        self.state = state;
        if let Some(saved) = saved_probs {
            self.state.probs = saved;
        }
        self.have_keyframe = true;
        if !tag.show_frame {
            return Ok(None);
        }
        self.shown = Some(target);
        Ok(Some(self.frame(target)))
    }

    /// Forget all reference frames (after a seek). The next frame must be a
    /// keyframe.
    pub fn reset(&mut self) {
        self.have_keyframe = false;
        self.shown = None;
    }

    fn frame(&self, index: usize) -> Frame<'_> {
        let f = &self.frames[index];
        let (w, h) = (self.width as usize, self.height as usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        Frame {
            width: self.width,
            height: self.height,
            y: &f.y.data[f.y.origin..][..(h - 1) * f.y.stride + w],
            u: &f.u.data[f.u.origin..][..(ch - 1) * f.u.stride + cw],
            v: &f.v.data[f.v.origin..][..(ch - 1) * f.v.stride + cw],
            y_stride: f.y.stride,
            uv_stride: f.u.stride,
        }
    }

    /// Set up for a new picture size. The keyframe that brings it replaces
    /// every reference, so the old buffers are simply dropped.
    fn resize(&mut self, tag: &FrameTag) -> Result<(), Error> {
        let mb_cols = (tag.width as usize).div_ceil(16);
        let mb_rows = (tag.height as usize).div_ceil(16);
        // A damaged header can claim up to 16383 x 16383, which would
        // allocate over a gigabyte. On a keyframe every macroblock costs at
        // least 3.4 bits of the first partition (its luma and chroma modes
        // are coded with fixed probabilities), so refuse sizes the partition
        // cannot describe before allocating anything.
        if mb_cols * mb_rows * 3 > tag.first_part_len * 8 {
            return Err(Error::Corrupt("frame size too large for its data"));
        }
        self.width = tag.width;
        self.height = tag.height;
        self.mb_cols = mb_cols;
        self.mb_rows = mb_rows;
        self.frames.clear();
        (self.last, self.golden, self.altref, self.shown) = (0, 0, 0, None);
        self.mbs = vec![MbInfo::BORDER; (mb_cols + 1) * (mb_rows + 1)];
        self.above_ctx = vec![[0; 9]; mb_cols];
        self.above = [vec![0; mb_cols * 16 + 4], vec![0; mb_cols * 8], vec![0; mb_cols * 8]];
        Ok(())
    }

    /// A buffer no reference and no shown frame is using.
    fn free_buffer(&mut self) -> usize {
        let used = [Some(self.last), Some(self.golden), Some(self.altref), self.shown];
        let free = (0..MAX_BUFFERS).find(|i| *i >= self.frames.len() || !used.contains(&Some(*i)));
        let free = free.expect("four users cannot hold five buffers");
        if free == self.frames.len() {
            self.frames.push(FrameBuf::default());
        }
        free
    }

    fn decode_frame(
        &mut self,
        tag: &FrameTag,
        header: &FrameHeader,
        state: &Persistent,
        mut bd: BoolDecoder,
        parts: [&[u8]; 8],
        cur: &mut FrameBuf,
    ) {
        let (mb_cols, mb_rows) = (self.mb_cols, self.mb_rows);
        let stride = mb_cols + 1;
        let dequant = Dequant::for_frame(header, state);
        let loop_filter = LoopFilter::new(header, state);
        // Versions 1 to 3 use bilinear prediction, and 3 whole-pixel chroma
        // vectors; reserved versions decode as 0, as in libvpx. (The version
        // also suggests a loop filter type, but the header bit decides.)
        let interp = Interp { bilinear: (1..=3).contains(&tag.version), full_pixel: tag.version == 3 };
        let parser = ModeParser { header, probs: &state.probs, mb_cols, mb_rows, stride };
        let mut token_bds = parts.map(BoolDecoder::new);
        let part_mask = (1usize << header.log2_partitions) - 1;
        let refs = [self.last, self.last, self.golden, self.altref];

        let Decoder { frames, mbs, above_ctx, above, coeffs, .. } = self;
        let [above_y, above_u, above_v] = above;
        above_ctx.fill([0; 9]);
        // Above the frame is 127, left of it 129.
        above_y.fill(127);
        above_u.fill(127);
        above_v.fill(127);

        for mb_row in 0..mb_rows {
            let tbd = &mut token_bds[mb_row & part_mask];
            let mut left_ctx: EdgeContext = [0; 9];
            let (mut left_y, mut left_u, mut left_v) = ([129u8; 16], [129u8; 8], [129u8; 8]);
            let corner = if mb_row == 0 { 127 } else { 129 };
            let (mut tl_y, mut tl_u, mut tl_v) = (corner, corner, corner);
            let row_start = (mb_row + 1) * stride + 1;

            for mb_col in 0..mb_cols {
                parser.parse(&mut bd, mbs, mb_col, mb_row);
                let mb = &mut mbs[row_start + mb_col];
                let dq = &dequant[if header.segmentation { usize::from(mb.segment & 3) } else { 0 }];
                let above_c = &mut above_ctx[mb_col];
                if mb.skip {
                    tokens::skip_mb(mb, above_c, &mut left_ctx);
                } else if !tokens::decode_mb(tbd, &state.probs.coeff, mb, dq, above_c, &mut left_ctx, coeffs) {
                    // Nothing was coded after all; the loop filter treats
                    // that the same as the flag.
                    mb.skip = true;
                }
                let mb = &*mb;

                let (x, y) = (mb_col * 16, mb_row * 16);
                let (cx, cy) = (mb_col * 8, mb_row * 8);
                let (y_at, ys) = (cur.y.at(x, y), cur.y.stride);
                let (c_at, cs) = (cur.u.at(cx, cy), cur.u.stride);
                let (have_above, have_left) = (mb_row > 0, mb_col > 0);

                if mb.ref_frame == INTRA_FRAME {
                    let au: &[u8; 8] = above_u[cx..].first_chunk().expect("row buffer covers the frame");
                    let av: &[u8; 8] = above_v[cx..].first_chunk().expect("row buffer covers the frame");
                    intra::predict_mb(&mut cur.u.data, c_at, cs, mb.uv_mode, au, &left_u, tl_u, have_above, have_left);
                    intra::predict_mb(&mut cur.v.data, c_at, cs, mb.uv_mode, av, &left_v, tl_v, have_above, have_left);
                    // 16 pixels above, then the 4 above-right.
                    let ay: &[u8; 20] = above_y[x..].first_chunk().expect("row buffer covers the frame");
                    if mb.mode == B_PRED {
                        predict_subblocks(&mut cur.y.data, y_at, ys, mb, ay, &left_y, tl_y, coeffs);
                    } else {
                        let ay: &[u8; 16] = ay.first_chunk().expect("16 of 20");
                        intra::predict_mb(&mut cur.y.data, y_at, ys, mb.mode, ay, &left_y, tl_y, have_above, have_left);
                    }
                } else {
                    let reference = &frames[refs[usize::from(mb.ref_frame & 3)]];
                    let bounds = MvBounds::new(mb_col, mb_row, mb_cols, mb_rows);
                    inter::predict_mb(reference, cur, mb_col, mb_row, mb, &bounds, interp);
                }

                if !mb.skip {
                    if mb.mode != B_PRED {
                        add_luma_residual(&mut cur.y.data, y_at, ys, mb, coeffs);
                    }
                    for i in 0..4 {
                        let at = c_at + (i >> 1) * 4 * cs + (i & 1) * 4;
                        add_block(&mut cur.u.data, at, cs, &coeffs.blocks[16 + i], coeffs.eob[16 + i] > 1);
                        add_block(&mut cur.v.data, at, cs, &coeffs.blocks[20 + i], coeffs.eob[20 + i] > 1);
                    }
                    coeffs.blocks = [[0; 16]; 25];
                }

                // Keep this macroblock's right column and bottom row,
                // unfiltered, for the intra prediction of its neighbours.
                tl_y = save_edges(&cur.y.data, y_at, ys, &mut above_y[x..x + 16], &mut left_y);
                tl_u = save_edges(&cur.u.data, c_at, cs, &mut above_u[cx..cx + 8], &mut left_u);
                tl_v = save_edges(&cur.v.data, c_at, cs, &mut above_v[cx..cx + 8], &mut left_v);
            }

            // Right of the frame, the row above continues with its last pixel.
            let w = mb_cols * 16;
            let edge = above_y[w - 1];
            above_y[w..].fill(edge);

            // Filtering a row touches only it and rows above it, and the
            // rows below predict from the unfiltered copies kept above, so
            // each row can be filtered as soon as it is reconstructed. (A
            // later parallel decoder can run this loop a row behind on
            // another thread.)
            if let Some(lf) = &loop_filter {
                lf.filter_row(cur, &mbs[row_start..row_start + mb_cols], mb_row);
            }
        }
        cur.extend_borders();
    }
}

/// Record a reconstructed block's bottom row in `above` and its right
/// column in `left`. Returns what was in `above`'s last pixel, which is the
/// above-left pixel of the next block to the right.
fn save_edges<const N: usize>(plane: &[u8], at: usize, stride: usize, above: &mut [u8], left: &mut [u8; N]) -> u8 {
    let corner = above[N - 1];
    for (l, row) in left.iter_mut().zip(plane[at..].chunks(stride)) {
        *l = row[N - 1];
    }
    above.copy_from_slice(&plane[at + (N - 1) * stride..][..N]);
    corner
}

/// Add one block's residual to its prediction: the full inverse DCT when
/// coefficients past DC may be present, else the DC shortcut.
#[inline(always)]
fn add_block(plane: &mut [u8], at: usize, stride: usize, block: &[i16; 16], has_ac: bool) {
    if has_ac {
        dsp::idct_add(block, plane, at, stride);
    } else if block[0] != 0 {
        dsp::idct_dc_add(block[0], plane, at, stride);
    }
}

/// Residual of a macroblock whose luma DC coefficients come through the Y2
/// block (every mode but B_PRED and SPLITMV), or directly (SPLITMV).
fn add_luma_residual(plane: &mut [u8], at: usize, stride: usize, mb: &MbInfo, coeffs: &mut Coeffs) {
    let has_y2 = mb.has_y2();
    if has_y2 {
        let dc = if coeffs.eob[Y2] > 1 {
            dsp::iwht(&coeffs.blocks[Y2])
        } else {
            // Only the DC of the Y2 block: its inverse transform is flat.
            [((i32::from(coeffs.blocks[Y2][0]) + 3) >> 3) as i16; 16]
        };
        for (block, dc) in coeffs.blocks.iter_mut().zip(dc) {
            block[0] = dc;
        }
    }
    for i in 0..16 {
        // With a Y2 block the tokens start at position 1, so any token
        // at all is an AC coefficient.
        let has_ac = coeffs.eob[i] > u8::from(!has_y2);
        add_block(plane, at + (i >> 2) * 4 * stride + (i & 3) * 4, stride, &coeffs.blocks[i], has_ac);
    }
}

/// Predict and reconstruct the 16 subblocks of a B_PRED macroblock in
/// order, each predicting from the ones before it. `above` is the 16 pixels
/// above the macroblock and the 4 above-right; subblocks on the right edge
/// in lower rows use those same 4 as their above-right.
#[allow(clippy::too_many_arguments)]
fn predict_subblocks(
    plane: &mut [u8],
    at: usize,
    stride: usize,
    mb: &MbInfo,
    above: &[u8; 20],
    left: &[u8; 16],
    top_left: u8,
    coeffs: &Coeffs,
) {
    for i in 0..16 {
        let (bx, by) = (i & 3, i >> 2);
        let block_at = at + by * 4 * stride + bx * 4;
        let mut a = [0u8; 8];
        let mut l = [0u8; 4];
        let corner = if by == 0 {
            a.copy_from_slice(&above[bx * 4..bx * 4 + 8]);
            if bx == 0 { top_left } else { above[bx * 4 - 1] }
        } else {
            let row_above = block_at - stride;
            if bx == 3 {
                a[..4].copy_from_slice(&plane[row_above..row_above + 4]);
                a[4..].copy_from_slice(&above[16..]);
            } else {
                a.copy_from_slice(&plane[row_above..row_above + 8]);
            }
            if bx == 0 { left[by * 4 - 1] } else { plane[row_above - 1] }
        };
        if bx == 0 {
            l.copy_from_slice(&left[by * 4..by * 4 + 4]);
        } else {
            for (r, p) in l.iter_mut().enumerate() {
                *p = plane[block_at + r * stride - 1];
            }
        }
        let rows = intra::predict_subblock(mb.bmodes[i], &a, &l, corner);
        for (r, row) in rows.iter().enumerate() {
            plane[block_at + r * stride..][..4].copy_from_slice(row);
        }
        if !mb.skip {
            add_block(plane, block_at, stride, &coeffs.blocks[i], coeffs.eob[i] > 1);
        }
    }
}
