//! VP8 through this project's own decoder (`crates/vp8`). WebM stores one
//! VP8 frame per block, which is what the decoder takes.

use airsplice_vp8::{Decoder, Frame, is_droppable, is_keyframe};
use anyhow::{Result, bail};

use super::yuv::{self, Planes, Subsampling};
use super::{DecodedFrame, VideoDecoder};

pub struct Vp8Decoder {
    decoder: Decoder,
    /// Frames before the first keyframe after a reset reference pictures the
    /// decoder never saw; they are skipped rather than shown as garbage.
    seen_keyframe: bool,
    discard_before: i64,
}

impl Vp8Decoder {
    pub fn new() -> Self {
        Self { decoder: Decoder::new(), seen_keyframe: false, discard_before: i64::MIN }
    }
}

impl Default for Vp8Decoder {
    fn default() -> Self {
        Self::new()
    }
}

fn convert(frame: &Frame<'_>, pts: i64) -> Result<DecodedFrame> {
    let (w, h) = (frame.width as usize, frame.height as usize);
    // The converter indexes the planes without checking; a picture that
    // does not fill them would panic the video thread.
    let fits = |plane: &[u8], stride: usize, w: usize, h: usize| {
        w > 0 && h > 0 && stride >= w && plane.len() >= (h - 1) * stride + w
    };
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    if !fits(frame.y, frame.y_stride, w, h)
        || !fits(frame.u, frame.uv_stride, cw, ch)
        || !fits(frame.v, frame.uv_stride, cw, ch)
    {
        bail!("VP8 decoder returned an incomplete {w}x{h} picture");
    }
    let bgra = yuv::to_bgra(&Planes {
        y: frame.y,
        u: frame.u,
        v: frame.v,
        y_stride: frame.y_stride,
        uv_stride: frame.uv_stride,
        width: w,
        height: h,
        subsampling: Subsampling::S420,
        bit_depth: 8,
    });
    Ok(DecodedFrame { pts, width: frame.width, height: frame.height, bgra })
}

impl VideoDecoder for Vp8Decoder {
    fn name(&self) -> &'static str {
        "airsplice-vp8 (software)"
    }

    fn is_keyframe(&self, data: &[u8]) -> bool {
        is_keyframe(data)
    }

    fn is_droppable(&self, data: &[u8]) -> bool {
        is_droppable(data)
    }

    fn decode(&mut self, data: &[u8], pts: i64) -> Result<Vec<DecodedFrame>> {
        if !self.seen_keyframe {
            if !is_keyframe(data) {
                return Ok(Vec::new());
            }
            self.seen_keyframe = true;
        }
        // Hidden frames (alternate references) come back as `None`: they
        // are stored as packets of their own and show nothing.
        let Some(frame) = self.decoder.decode(data)? else { return Ok(Vec::new()) };
        if pts < self.discard_before {
            return Ok(Vec::new());
        }
        Ok(vec![convert(&frame, pts)?])
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        // Every shown frame is returned by the call that decoded it.
        Ok(Vec::new())
    }

    fn reset(&mut self, discard_before: i64) -> Result<()> {
        self.decoder.reset();
        self.seen_keyframe = false;
        self.discard_before = discard_before;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A keyframe header whose first partition is longer than the data.
    const KEY: [u8; 10] = [0xf0, 0xff, 0xff, 0x9d, 0x01, 0x2a, 0x40, 0x01, 0xf0, 0x00];

    #[test]
    fn keyframe_detection() {
        let dec = Vp8Decoder::new();
        assert!(dec.is_keyframe(&KEY));
        // Bit 0 of the frame tag set: an inter frame.
        assert!(!dec.is_keyframe(&[0x11, 0, 0, 0x9d, 0x01, 0x2a, 0x40, 0x01, 0xf0, 0x00]));
        // Wrong start code, a truncated header, a dropped (empty) frame.
        assert!(!dec.is_keyframe(&[0x10, 0, 0, 0x9d, 0x01, 0x2b, 0x40, 0x01, 0xf0, 0x00]));
        assert!(!dec.is_keyframe(&KEY[..9]));
        assert!(!dec.is_keyframe(&[]));
    }

    #[test]
    fn frames_before_the_first_keyframe_are_skipped_without_error() {
        // A stream cut mid-GOP: nothing to show, and nothing to report.
        let mut dec = Vp8Decoder::new();
        for data in [&[0x11u8, 0x02, 0x00, 0x55, 0x55][..], &[]] {
            assert!(dec.decode(data, 0).unwrap().is_empty());
        }
    }

    #[test]
    fn a_reset_waits_for_the_next_keyframe() {
        let mut dec = Vp8Decoder::new();
        dec.seen_keyframe = true;
        dec.reset(40).unwrap();
        assert!(dec.decode(&[0x11, 0x02, 0x00, 0x55, 0x55], 80).unwrap().is_empty());
        // The keyframe is handed to the decoder, which finds it truncated.
        let err = dec.decode(&KEY, 120).err().expect("a truncated keyframe is an error");
        assert!(err.to_string().contains("VP8"), "{err}");
    }

    #[test]
    fn incomplete_pictures_are_refused() {
        let (y, c) = ([0u8; 12], [0u8; 2]);
        let frame = |width, height| Frame { width, height, y: &y, u: &c, v: &c, y_stride: 4, uv_stride: 2 };
        assert_eq!(convert(&frame(3, 2), 7).map(|f| (f.pts, f.bgra.len())).unwrap(), (7, 24));
        assert!(convert(&frame(0, 2), 0).is_err(), "no picture");
        assert!(convert(&frame(4, 4), 0).is_err(), "more rows than the planes hold");
    }
}
