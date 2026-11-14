//! AV1 through rav1d, the Rust port of dav1d. rav1d exposes the dav1d API, so
//! this module is a thin safe wrapper around it.

use std::mem::MaybeUninit;
use std::ptr::NonNull;

use anyhow::{Result, anyhow, bail};
use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::headers::{
    DAV1D_PIXEL_LAYOUT_I400, DAV1D_PIXEL_LAYOUT_I420, DAV1D_PIXEL_LAYOUT_I422, DAV1D_PIXEL_LAYOUT_I444,
};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::{
    dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings, dav1d_flush, dav1d_get_picture,
    dav1d_open, dav1d_picture_unref, dav1d_send_data,
};

use super::yuv::{self, Planes, Subsampling};
use super::{DecodedFrame, VideoDecoder};

/// dav1d returns errno values negated; EAGAIN differs per platform.
const EAGAIN: i32 = -libc::EAGAIN;

pub struct Av1Decoder {
    ctx: Option<Dav1dContext>,
    /// Sequence header OBUs from the `av1C` record, re-sent after a flush.
    config_obus: Vec<u8>,
    seen_keyframe: bool,
    discard_before: i64,
}

const OBU_FRAME_HEADER: u8 = 3;
const OBU_FRAME: u8 = 6;

fn leb128(data: &[u8]) -> Option<(usize, usize)> {
    let mut value = 0usize;
    for (i, b) in data.iter().take(8).enumerate() {
        value |= ((b & 0x7f) as usize) << (i * 7);
        if b & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// Whether a temporal unit (low-overhead OBU format, as stored in MP4 and
/// Matroska) starts with a shown key frame.
pub fn is_keyframe(data: &[u8]) -> bool {
    let mut pos = 0;
    while pos < data.len() {
        let header = data[pos];
        let obu_type = (header >> 3) & 0x0f;
        let has_extension = header & 0x04 != 0;
        let has_size = header & 0x02 != 0;
        pos += 1 + usize::from(has_extension);
        let size = if has_size {
            let Some((size, n)) = data.get(pos..).and_then(leb128) else { return false };
            pos += n;
            size
        } else {
            data.len().saturating_sub(pos)
        };
        if obu_type == OBU_FRAME_HEADER || obu_type == OBU_FRAME {
            // Non-reduced header: show_existing_frame(1), frame_type(2).
            let Some(&b) = data.get(pos) else { return false };
            return b & 0x80 == 0 && (b >> 5) & 0x03 == 0;
        }
        pos += size;
    }
    false
}

// SAFETY: the dav1d context is internally synchronised and only used from the
// thread that owns this decoder.
unsafe impl Send for Av1Decoder {}

impl Av1Decoder {
    pub fn new(av1c: Option<&[u8]>) -> Result<Self> {
        let mut settings = MaybeUninit::<Dav1dSettings>::uninit();
        // SAFETY: dav1d_default_settings fully initialises the struct.
        let mut settings = unsafe {
            dav1d_default_settings(NonNull::new_unchecked(settings.as_mut_ptr()));
            settings.assume_init()
        };
        settings.n_threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8)) as i32;
        settings.max_frame_delay = 2;
        let mut ctx: Option<Dav1dContext> = None;
        // SAFETY: both pointers are valid for the duration of the call.
        let res = unsafe { dav1d_open(Some(NonNull::from(&mut ctx)), Some(NonNull::from(&mut settings))) };
        if res.0 != 0 || ctx.is_none() {
            bail!("rav1d: dav1d_open failed ({})", res.0);
        }
        // av1C: 4-byte header, then optional configOBUs.
        let config_obus = av1c.filter(|c| c.len() > 4).map(|c| c[4..].to_vec()).unwrap_or_default();
        let mut dec = Self { ctx, config_obus, seen_keyframe: true, discard_before: i64::MIN };
        dec.send_config()?;
        Ok(dec)
    }

    fn send_config(&mut self) -> Result<()> {
        if !self.config_obus.is_empty() {
            let obus = self.config_obus.clone();
            let mut sink = Vec::new();
            self.send(&obus, 0, &mut sink)?;
        }
        Ok(())
    }

    fn send(&mut self, bytes: &[u8], pts: i64, out: &mut Vec<DecodedFrame>) -> Result<()> {
        let mut data = Dav1dData::default();
        // SAFETY: dav1d_data_create allocates `bytes.len()` writable bytes.
        let ptr = unsafe { dav1d_data_create(Some(NonNull::from(&mut data)), bytes.len()) };
        if ptr.is_null() {
            bail!("rav1d: out of memory");
        }
        // SAFETY: ptr points to bytes.len() bytes owned by `data`.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len()) };
        data.m.timestamp = pts;

        // dav1d's reference loop: keep offering the data, draining pictures
        // between attempts, until the decoder has consumed all of it.
        let mut result = Ok(());
        while data.sz > 0 {
            // SAFETY: valid context and data.
            let res = unsafe { dav1d_send_data(self.ctx, Some(NonNull::from(&mut data))) };
            if res.0 != 0 && res.0 != EAGAIN {
                // Missing references right after a seek land here; skip the unit.
                log::debug!("rav1d: send_data error {}", res.0);
                break;
            }
            if let Err(e) = self.drain(out) {
                result = Err(e);
                break;
            }
        }
        if data.sz != 0 {
            // SAFETY: data still owns its buffer.
            unsafe { dav1d_data_unref(Some(NonNull::from(&mut data))) };
        }
        result
    }

    fn drain(&mut self, out: &mut Vec<DecodedFrame>) -> Result<()> {
        loop {
            let mut pic = Dav1dPicture::default();
            // SAFETY: valid context; pic is a valid out-param.
            let res = unsafe { dav1d_get_picture(self.ctx, Some(NonNull::from(&mut pic))) };
            if res.0 == EAGAIN {
                return Ok(());
            }
            if res.0 != 0 {
                log::debug!("rav1d: get_picture error {}", res.0);
                return Ok(());
            }
            let frame = if pic.m.timestamp < self.discard_before { None } else { Some(convert(&pic)) };
            // SAFETY: pic was filled by dav1d_get_picture.
            unsafe { dav1d_picture_unref(Some(NonNull::from(&mut pic))) };
            if let Some(frame) = frame {
                out.push(frame?);
            }
        }
    }
}

fn convert(pic: &Dav1dPicture) -> Result<DecodedFrame> {
    let (w, h) = (pic.p.w as usize, pic.p.h as usize);
    let subsampling = match pic.p.layout {
        DAV1D_PIXEL_LAYOUT_I420 => Subsampling::S420,
        DAV1D_PIXEL_LAYOUT_I422 => Subsampling::S422,
        DAV1D_PIXEL_LAYOUT_I444 => Subsampling::S444,
        DAV1D_PIXEL_LAYOUT_I400 => Subsampling::Mono,
        other => bail!("rav1d: unknown pixel layout {other}"),
    };
    let ch = if subsampling == Subsampling::S420 { h.div_ceil(2) } else { h };
    let (ys, cs) = (pic.stride[0] as usize, pic.stride[1] as usize);
    let plane = |i: usize| -> Result<*const u8> {
        let p = pic.data[i].ok_or_else(|| anyhow!("rav1d: missing plane {i}"))?;
        Ok(p.as_ptr() as *const u8)
    };
    let mono = subsampling == Subsampling::Mono;
    let y_ptr = plane(0)?;
    let (u_ptr, v_ptr) = if mono { (y_ptr, y_ptr) } else { (plane(1)?, plane(2)?) };
    let (uv_len, uv_stride_bytes) = if mono { (ys * h, ys) } else { (cs * ch, cs) };
    let bgra = if pic.p.bpc > 8 {
        // SAFETY: dav1d planes are valid for stride*rows bytes while `pic`
        // is referenced, and 16-bit planes are 2-byte aligned.
        let (y, u, v) = unsafe {
            (
                std::slice::from_raw_parts(y_ptr as *const u16, ys * h / 2),
                std::slice::from_raw_parts(u_ptr as *const u16, uv_len / 2),
                std::slice::from_raw_parts(v_ptr as *const u16, uv_len / 2),
            )
        };
        yuv::to_bgra(&Planes {
            y,
            u,
            v,
            y_stride: ys / 2,
            uv_stride: uv_stride_bytes / 2,
            width: w,
            height: h,
            subsampling,
            bit_depth: pic.p.bpc as u32,
        })
    } else {
        // SAFETY: as above.
        let (y, u, v) = unsafe {
            (
                std::slice::from_raw_parts(y_ptr, ys * h),
                std::slice::from_raw_parts(u_ptr, uv_len),
                std::slice::from_raw_parts(v_ptr, uv_len),
            )
        };
        yuv::to_bgra(&Planes {
            y,
            u,
            v,
            y_stride: ys,
            uv_stride: uv_stride_bytes,
            width: w,
            height: h,
            subsampling,
            bit_depth: 8,
        })
    };
    Ok(DecodedFrame { pts: pic.m.timestamp, width: w as u32, height: h as u32, bgra })
}

impl VideoDecoder for Av1Decoder {
    fn is_keyframe(&self, data: &[u8]) -> bool {
        is_keyframe(data)
    }

    fn decode(&mut self, data: &[u8], pts: i64) -> Result<Vec<DecodedFrame>> {
        if !self.seen_keyframe {
            if !is_keyframe(data) {
                return Ok(Vec::new());
            }
            self.seen_keyframe = true;
        }
        let mut out = Vec::new();
        self.send(data, pts, &mut out)?;
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        let mut out = Vec::new();
        self.drain(&mut out)?;
        Ok(out)
    }

    fn reset(&mut self, discard_before: i64) -> Result<()> {
        // SAFETY: valid context.
        unsafe { dav1d_flush(self.ctx.expect("open context")) };
        self.seen_keyframe = false;
        self.discard_before = discard_before;
        self.send_config()
    }
}

impl Drop for Av1Decoder {
    fn drop(&mut self) {
        // SAFETY: closes the context we opened; dav1d_close nulls it.
        unsafe { dav1d_close(Some(NonNull::from(&mut self.ctx))) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframe_detection() {
        // Temporal delimiter (type 2, size 0), then a FRAME OBU (type 6) whose
        // first payload byte has show_existing_frame=0, frame_type=0 (KEY).
        assert!(is_keyframe(&[0x12, 0x00, 0x32, 0x01, 0x10]));
        // Same, frame_type=1 (INTER): bits 0 01 -> 0x20.
        assert!(!is_keyframe(&[0x12, 0x00, 0x32, 0x01, 0x20]));
        // show_existing_frame=1.
        assert!(!is_keyframe(&[0x12, 0x00, 0x32, 0x01, 0x80]));
        // Only a temporal delimiter.
        assert!(!is_keyframe(&[0x12, 0x00]));
        assert_eq!(leb128(&[0x80, 0x01]), Some((128, 2)));
    }
}
