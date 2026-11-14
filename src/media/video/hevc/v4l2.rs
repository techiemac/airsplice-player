//! Stateless V4L2 HEVC decoding (the Raspberry Pi's `rpi-hevc-dec`).
//!
//! One picture per media request: its slice data goes into an OUTPUT buffer,
//! its controls (SPS, PPS, slice parameters, decode parameters, scaling
//! matrix) are attached to the request, and the picture lands in a CAPTURE
//! buffer in the Pi's column-tiled layout. Capture buffers holding reference
//! pictures are kept out of rotation until the stream stops referencing them.
//!
//! The cros-codecs parser is single-threaded (`Rc`), so all of this runs on a
//! worker thread; the `VideoDecoder` handle talks to it over channels.

use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use std::mem::size_of;

use anyhow::{Context as _, Result, anyhow, bail};
use v4l2r::bindings as b;

use super::col128::Col128;
use super::frames::{FrameControls, HevcFrames, tag_to_ns};
use super::uapi;
use crate::media::video::{DecodedFrame, KeyframeCheck, Reorder, VideoDecoder};

// The kernel ABI these mirrors must match (checked against the headers the
// build compiled against).
const _: () = {
    use std::mem::size_of;
    assert!(size_of::<uapi::Sps>() == size_of::<b::v4l2_ctrl_hevc_sps>());
    assert!(size_of::<uapi::Pps>() == size_of::<b::v4l2_ctrl_hevc_pps>());
    assert!(size_of::<uapi::SliceParams>() == size_of::<b::v4l2_ctrl_hevc_slice_params>());
    assert!(size_of::<uapi::DecodeParams>() == size_of::<b::v4l2_ctrl_hevc_decode_params>());
    assert!(size_of::<uapi::ScalingMatrix>() == size_of::<b::v4l2_ctrl_hevc_scaling_matrix>());
};

const fn fourcc(s: &[u8; 4]) -> u32 {
    (s[0] as u32) | (s[1] as u32) << 8 | (s[2] as u32) << 16 | (s[3] as u32) << 24
}
/// `V4L2_PIX_FMT_HEVC_SLICE`: parsed slice data.
const FMT_HEVC_SLICE: u32 = fourcc(b"S265");
/// Raspberry Pi kernel additions, absent from mainline headers.
const FMT_NV12MT_COL128: u32 = fourcc(b"Nc12");
const FMT_NV12MT_10_COL128: u32 = fourcc(b"Nc30");

const CID_STATELESS_BASE: u32 = 0x00a4_0900;
const CID_HEVC_SPS: u32 = CID_STATELESS_BASE + 400;
const CID_HEVC_PPS: u32 = CID_STATELESS_BASE + 401;
const CID_HEVC_SLICE_PARAMS: u32 = CID_STATELESS_BASE + 402;
const CID_HEVC_SCALING_MATRIX: u32 = CID_STATELESS_BASE + 403;
const CID_HEVC_DECODE_PARAMS: u32 = CID_STATELESS_BASE + 404;
const CID_HEVC_DECODE_MODE: u32 = CID_STATELESS_BASE + 405;
const CID_HEVC_START_CODE: u32 = CID_STATELESS_BASE + 406;
const DECODE_MODE_FRAME_BASED: i32 = 1;
const START_CODE_NONE: i32 = 0;

const OUTPUT: u32 = b::v4l2_buf_type_V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
const CAPTURE: u32 = b::v4l2_buf_type_V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
const MMAP: u32 = b::v4l2_memory_V4L2_MEMORY_MMAP;
const OUTPUT_BUFFERS: u32 = 2;
/// Capture buffers beyond the stream's DPB: the picture being decoded plus
/// slack for the driver.
const EXTRA_CAPTURE_BUFFERS: usize = 3;
/// Generous: a 4K picture decodes in a few milliseconds.
const DECODE_TIMEOUT_MS: i32 = 2000;

mod ioctl {
    use super::b;
    nix::ioctl_read!(querycap, b'V', 0, b::v4l2_capability);
    nix::ioctl_readwrite!(enum_fmt, b'V', 2, b::v4l2_fmtdesc);
    nix::ioctl_readwrite!(s_fmt, b'V', 5, b::v4l2_format);
    nix::ioctl_readwrite!(reqbufs, b'V', 8, b::v4l2_requestbuffers);
    nix::ioctl_readwrite!(querybuf, b'V', 9, b::v4l2_buffer);
    nix::ioctl_readwrite!(qbuf, b'V', 15, b::v4l2_buffer);
    nix::ioctl_readwrite!(dqbuf, b'V', 17, b::v4l2_buffer);
    nix::ioctl_write_ptr!(streamon, b'V', 18, libc::c_int);
    nix::ioctl_write_ptr!(streamoff, b'V', 19, libc::c_int);
    nix::ioctl_readwrite!(s_ext_ctrls, b'V', 72, b::v4l2_ext_controls);
    nix::ioctl_read!(media_request_alloc, b'|', 0x05, libc::c_int);
    nix::ioctl_none!(media_request_queue, b'|', 0x80);
    nix::ioctl_none!(media_request_reinit, b'|', 0x81);
}

/// A memory-mapped plane of a V4L2 buffer.
struct Mapping {
    ptr: *mut u8,
    len: usize,
}

impl Mapping {
    fn new(fd: RawFd, offset: u32, len: u32) -> Result<Self> {
        // SAFETY: mapping a buffer the driver exported at `offset`.
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), len as usize, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, offset as libc::off_t)
        };
        if ptr == libc::MAP_FAILED {
            bail!("mmap: {}", std::io::Error::last_os_error());
        }
        Ok(Self { ptr: ptr.cast(), len: len as usize })
    }

    fn slice(&self) -> &[u8] {
        // SAFETY: a live mapping of `len` bytes.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    fn slice_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above; we only write while the driver does not own it.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmapping our own mapping.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

/// The decoder's video device and the media device its requests go to.
pub struct DeviceNodes {
    pub video: PathBuf,
    pub media: PathBuf,
}

/// Find a stateless HEVC decoder: a memory-to-memory device accepting
/// `S265`, plus its media device from sysfs.
pub fn find_device() -> Option<DeviceNodes> {
    let mut entries: Vec<_> = std::fs::read_dir("/dev").ok()?.flatten().map(|e| e.path()).collect();
    entries.sort();
    entries.into_iter().filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("video"))).find_map(|video| {
        let file = File::open(&video).ok()?;
        accepts_hevc_slices(&file).then_some(())?;
        let name = video.file_name()?.to_str()?.to_string();
        let sys = Path::new("/sys/class/video4linux").join(&name).join("device");
        let media = std::fs::read_dir(sys).ok()?.flatten().find_map(|e| {
            let n = e.file_name().into_string().ok()?;
            n.starts_with("media").then(|| Path::new("/dev").join(n))
        })?;
        Some(DeviceNodes { video, media })
    })
}

fn accepts_hevc_slices(file: &File) -> bool {
    let mut cap = b::v4l2_capability::default();
    // SAFETY: valid fd and out-param.
    if unsafe { ioctl::querycap(file.as_raw_fd(), &mut cap) }.is_err() {
        return false;
    }
    let caps = if cap.capabilities & b::V4L2_CAP_DEVICE_CAPS != 0 { cap.device_caps } else { cap.capabilities };
    if caps & b::V4L2_CAP_VIDEO_M2M_MPLANE == 0 {
        return false;
    }
    (0..32).any(|index| {
        let mut desc = b::v4l2_fmtdesc { index, type_: OUTPUT, ..Default::default() };
        // SAFETY: valid fd and in/out-param.
        unsafe { ioctl::enum_fmt(file.as_raw_fd(), &mut desc) }.is_ok() && desc.pixelformat == FMT_HEVC_SLICE
    })
}

struct CaptureBuffer {
    planes: [Mapping; 2],
    /// The picture it holds, while that picture may still be referenced.
    tag: Option<u64>,
}

struct Device {
    video: File,
    media: File,
    output: Vec<Mapping>,
    capture: Vec<CaptureBuffer>,
    requests: Vec<OwnedFd>,
    width: u32,
    height: u32,
    /// Rows per column in the capture layout (aligned height).
    column_height: usize,
    ten_bit: bool,
    /// Reused across pictures: plane copies out of the device mappings.
    scratch: std::cell::RefCell<(Vec<u8>, Vec<u8>)>,
}

/// Set one plain integer control outside any request.
fn set_int_control(fd: RawFd, id: u32, value: i32) -> Result<()> {
    let mut ctrl = b::v4l2_ext_control { id, ..Default::default() };
    ctrl.__bindgen_anon_1.value = value;
    let mut ctrls = b::v4l2_ext_controls { count: 1, controls: &mut ctrl, ..Default::default() };
    ctrls.__bindgen_anon_1.which = b::V4L2_CTRL_WHICH_CUR_VAL;
    // SAFETY: valid fd; `ctrl` outlives the call.
    unsafe { ioctl::s_ext_ctrls(fd, &mut ctrls) }.map(drop).with_context(|| format!("setting control {id:#x}"))
}

/// A compound control: `id` with `size` bytes at `ptr`.
fn compound(id: u32, ptr: *const u8, size: usize) -> b::v4l2_ext_control {
    let mut ctrl = b::v4l2_ext_control { id, size: size as u32, ..Default::default() };
    ctrl.__bindgen_anon_1.ptr = ptr as *mut libc::c_void;
    ctrl
}

fn as_ptr<T>(v: &T) -> *const u8 {
    (v as *const T).cast()
}

impl Device {
    fn open(nodes: &DeviceNodes, frame: &FrameControls) -> Result<Self> {
        let open = |p: &Path| OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(p);
        let video = open(&nodes.video).with_context(|| format!("opening {}", nodes.video.display()))?;
        let media = open(&nodes.media).with_context(|| format!("opening {}", nodes.media.display()))?;
        let fd = video.as_raw_fd();
        let (width, height) = (frame.width, frame.height);

        // Compressed input: one plane, sized for the largest picture.
        let mut fmt = b::v4l2_format { type_: OUTPUT, ..Default::default() };
        // SAFETY: writing the active union member for an MPLANE type.
        unsafe {
            let pix = &mut fmt.fmt.pix_mp;
            pix.width = width;
            pix.height = height;
            pix.pixelformat = FMT_HEVC_SLICE;
            pix.num_planes = 1;
            pix.plane_fmt[0].sizeimage = (width * height * 3 / 2).max(1 << 20);
            ioctl::s_fmt(fd, &mut fmt).context("setting the input format")?;
        }
        set_int_control(fd, CID_HEVC_DECODE_MODE, DECODE_MODE_FRAME_BASED)?;
        set_int_control(fd, CID_HEVC_START_CODE, START_CODE_NONE)?;
        // The SPS decides which capture formats are valid (bit depth).
        let mut sps_ctrl = compound(CID_HEVC_SPS, as_ptr(&frame.sps), size_of::<uapi::Sps>());
        let mut ctrls = b::v4l2_ext_controls { count: 1, controls: &mut sps_ctrl, ..Default::default() };
        ctrls.__bindgen_anon_1.which = b::V4L2_CTRL_WHICH_CUR_VAL;
        // SAFETY: valid fd; the SPS outlives the call.
        unsafe { ioctl::s_ext_ctrls(fd, &mut ctrls) }.context("setting the SPS")?;

        let ten_bit = frame.bit_depth > 8;
        let mut fmt = b::v4l2_format { type_: CAPTURE, ..Default::default() };
        // SAFETY: as above.
        let column_height = unsafe {
            let pix = &mut fmt.fmt.pix_mp;
            pix.width = width;
            pix.height = height;
            pix.pixelformat = if ten_bit { FMT_NV12MT_10_COL128 } else { FMT_NV12MT_COL128 };
            pix.num_planes = 2;
            ioctl::s_fmt(fd, &mut fmt).context("setting the output picture format")?;
            let pix = &fmt.fmt.pix_mp;
            if pix.num_planes != 2 {
                bail!("decoder returned a {}-plane picture format", pix.num_planes);
            }
            pix.height as usize
        };

        let mut dev = Self {
            video,
            media,
            output: Vec::new(),
            capture: Vec::new(),
            requests: Vec::new(),
            width,
            height,
            column_height,
            ten_bit,
            scratch: Default::default(),
        };
        dev.output = dev.allocate(OUTPUT, OUTPUT_BUFFERS, 1)?.into_iter().map(|mut p| p.remove(0)).collect();
        let count = (frame.max_dpb + EXTRA_CAPTURE_BUFFERS) as u32;
        dev.capture = dev
            .allocate(CAPTURE, count, 2)?
            .into_iter()
            .map(|mut planes| {
                let chroma = planes.remove(1);
                let luma = planes.remove(0);
                CaptureBuffer { planes: [luma, chroma], tag: None }
            })
            .collect();
        for _ in 0..OUTPUT_BUFFERS {
            let mut req: libc::c_int = -1;
            // SAFETY: valid media fd and out-param.
            unsafe { ioctl::media_request_alloc(dev.media.as_raw_fd(), &mut req) }.context("allocating a media request")?;
            // SAFETY: the kernel returned a new fd we now own.
            dev.requests.push(unsafe { OwnedFd::from_raw_fd(req) });
        }
        for t in [OUTPUT, CAPTURE] {
            let t = t as libc::c_int;
            // SAFETY: valid fd.
            unsafe { ioctl::streamon(fd, &t) }.context("starting streaming")?;
        }
        Ok(dev)
    }

    /// REQBUFS + QUERYBUF + mmap; returns each buffer's plane mappings.
    fn allocate(&self, type_: u32, count: u32, planes: usize) -> Result<Vec<Vec<Mapping>>> {
        let fd = self.video.as_raw_fd();
        let mut req = b::v4l2_requestbuffers { count, type_, memory: MMAP, ..Default::default() };
        // SAFETY: valid fd and in/out-param.
        unsafe { ioctl::reqbufs(fd, &mut req) }.context("allocating buffers")?;
        let mut out = Vec::new();
        for index in 0..req.count {
            let mut plane_info = [b::v4l2_plane::default(); 2];
            let mut buf = b::v4l2_buffer { index, type_, memory: MMAP, length: planes as u32, ..Default::default() };
            buf.m.planes = plane_info.as_mut_ptr();
            // SAFETY: `plane_info` has room for `length` planes.
            unsafe { ioctl::querybuf(fd, &mut buf) }.context("querying a buffer")?;
            let maps = plane_info[..planes]
                .iter()
                // SAFETY: MMAP buffers report `mem_offset`.
                .map(|p| Mapping::new(fd, unsafe { p.m.mem_offset }, p.length))
                .collect::<Result<Vec<_>>>()?;
            out.push(maps);
        }
        Ok(out)
    }

    fn queue_capture(&self, index: usize) -> Result<()> {
        let mut planes = [b::v4l2_plane::default(); 2];
        let mut buf = b::v4l2_buffer { index: index as u32, type_: CAPTURE, memory: MMAP, length: 2, ..Default::default() };
        buf.m.planes = planes.as_mut_ptr();
        // SAFETY: valid fd; `planes` outlives the call.
        unsafe { ioctl::qbuf(self.video.as_raw_fd(), &mut buf) }.context("queueing a picture buffer")?;
        Ok(())
    }

    /// Wait for the device to hand back a buffer of `type_`.
    fn dequeue(&self, type_: u32, planes: u32) -> Result<b::v4l2_buffer> {
        let mut plane_info = [b::v4l2_plane::default(); 2];
        let mut buf = b::v4l2_buffer { type_, memory: MMAP, length: planes, ..Default::default() };
        buf.m.planes = plane_info.as_mut_ptr();
        // SAFETY: valid fd; `plane_info` outlives the call.
        unsafe { ioctl::dqbuf(self.video.as_raw_fd(), &mut buf) }.context("dequeueing a buffer")?;
        buf.m.planes = std::ptr::null_mut();
        Ok(buf)
    }

    /// Decode one picture into a free capture buffer; returns its index.
    fn decode(&mut self, frame: &FrameControls) -> Result<usize> {
        // Pictures no longer referenced free their buffers.
        let live: Vec<u64> = frame.live_tags.clone();
        for c in &mut self.capture {
            if c.tag.is_some_and(|t| !live.contains(&t)) {
                c.tag = None;
            }
        }
        let target = self
            .capture
            .iter()
            .position(|c| c.tag.is_none())
            .ok_or_else(|| anyhow!("no free picture buffer (DPB larger than declared)"))?;
        self.queue_capture(target)?;

        let out_index = (frame.tag as usize) % self.output.len();
        let dst = self.output[out_index].slice_mut();
        if frame.data.len() > dst.len() {
            bail!("picture data ({} bytes) exceeds the input buffer ({})", frame.data.len(), dst.len());
        }
        dst[..frame.data.len()].copy_from_slice(&frame.data);

        let request = self.requests[out_index].as_raw_fd();
        let mut ctrls = vec![
            compound(CID_HEVC_SPS, as_ptr(&frame.sps), size_of::<uapi::Sps>()),
            compound(CID_HEVC_PPS, as_ptr(&frame.pps), size_of::<uapi::Pps>()),
            compound(CID_HEVC_DECODE_PARAMS, as_ptr(&frame.decode), size_of::<uapi::DecodeParams>()),
            compound(
                CID_HEVC_SLICE_PARAMS,
                frame.slices.as_ptr().cast(),
                std::mem::size_of_val(frame.slices.as_slice()),
            ),
        ];
        if let Some(m) = &frame.scaling {
            ctrls.push(compound(CID_HEVC_SCALING_MATRIX, as_ptr(m), size_of::<uapi::ScalingMatrix>()));
        }
        let mut ext = b::v4l2_ext_controls {
            count: ctrls.len() as u32,
            request_fd: request,
            controls: ctrls.as_mut_ptr(),
            ..Default::default()
        };
        ext.__bindgen_anon_1.which = b::V4L2_CTRL_WHICH_REQUEST_VAL;
        // SAFETY: valid fds; every control payload outlives the call.
        unsafe { ioctl::s_ext_ctrls(self.video.as_raw_fd(), &mut ext) }.context("attaching picture controls")?;

        let mut plane = b::v4l2_plane { bytesused: frame.data.len() as u32, ..Default::default() };
        let mut buf = b::v4l2_buffer {
            index: out_index as u32,
            type_: OUTPUT,
            memory: MMAP,
            length: 1,
            flags: b::V4L2_BUF_FLAG_REQUEST_FD,
            ..Default::default()
        };
        buf.m.planes = &mut plane;
        buf.__bindgen_anon_1.request_fd = request;
        let ns = tag_to_ns(frame.tag);
        buf.timestamp.tv_sec = (ns / 1_000_000_000) as _;
        buf.timestamp.tv_usec = ((ns / 1000) % 1_000_000) as _;
        // SAFETY: valid fd; `plane` outlives the call.
        unsafe { ioctl::qbuf(self.video.as_raw_fd(), &mut buf) }.context("queueing picture data")?;
        // SAFETY: valid request fd.
        unsafe { ioctl::media_request_queue(request) }.context("submitting the decode request")?;

        let mut pfd = libc::pollfd { fd: request, events: libc::POLLPRI, revents: 0 };
        // SAFETY: one valid pollfd.
        let ready = unsafe { libc::poll(&mut pfd, 1, DECODE_TIMEOUT_MS) };
        if ready <= 0 {
            bail!("decoder did not finish a picture within {DECODE_TIMEOUT_MS} ms");
        }
        self.dequeue(OUTPUT, 1)?;
        let done = self.dequeue(CAPTURE, 2)?;
        // SAFETY: valid request fd.
        unsafe { ioctl::media_request_reinit(request) }.context("recycling the decode request")?;
        if done.index as usize != target {
            bail!("decoder returned buffer {} for picture in buffer {target}", done.index);
        }
        if done.flags & b::V4L2_BUF_FLAG_ERROR != 0 {
            bail!("decoder reported an error for this picture");
        }
        self.capture[target].tag = Some(frame.tag);
        Ok(target)
    }

    fn picture(&self, index: usize) -> Vec<u8> {
        let c = &self.capture[index];
        // One sequential copy out of the (typically uncached) mapping; the
        // scattered reads of de-tiling then hit cached memory.
        let mut scratch = self.scratch.borrow_mut();
        let (luma, chroma) = &mut *scratch;
        parallel_copy(c.planes[0].slice(), luma);
        parallel_copy(c.planes[1].slice(), chroma);
        Col128 {
            luma,
            chroma,
            width: self.width as usize,
            height: self.height as usize,
            column_height: self.column_height,
            ten_bit: self.ten_bit,
        }
        .to_bgra()
    }

    fn release_all(&mut self) {
        for c in &mut self.capture {
            c.tag = None;
        }
    }
}

/// Copy out of a device mapping on all cores: uncached reads are limited by
/// per-core latency, so splitting the copy scales with the core count.
fn parallel_copy(src: &[u8], out: &mut Vec<u8>) {
    use rayon::prelude::*;
    out.resize(src.len(), 0);
    out.par_chunks_mut(1 << 18).zip(src.par_chunks(1 << 18)).for_each(|(d, s)| d.copy_from_slice(s));
}

impl Drop for Device {
    fn drop(&mut self) {
        let fd = self.video.as_raw_fd();
        for t in [OUTPUT, CAPTURE] {
            let t = t as libc::c_int;
            // SAFETY: valid fd; failures here are harmless at teardown.
            let _ = unsafe { ioctl::streamoff(fd, &t) };
        }
    }
}

enum Command {
    Decode { data: Vec<u8>, pts: i64 },
    Flush,
    Reset { discard_before: i64 },
}

/// Runs on the worker thread.
struct Worker {
    nodes: DeviceNodes,
    frames: HevcFrames,
    device: Option<Device>,
    /// Display order: the reorder depth comes from the SPS.
    reorder: Reorder,
    discard_before: i64,
}

impl Worker {
    fn decode(&mut self, data: &[u8], pts: i64) -> Result<Vec<DecodedFrame>> {
        // A malformed (or merely unusual) stream must not take the decoder
        // thread down with a parser panic: report it and resync instead.
        let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.frames.frame(data)));
        let parsed = match parsed {
            Ok(result) => result,
            Err(_) => {
                self.frames.reset();
                bail!("HEVC parser failed on this picture; resuming at the next keyframe");
            }
        };
        let Some(frame) = parsed? else { return Ok(Vec::new()) };
        let reconfigure = self.device.as_ref().is_none_or(|d| {
            (d.width, d.height, d.ten_bit) != (frame.width, frame.height, frame.bit_depth > 8)
        });
        if reconfigure {
            self.device = None; // close the old one first
            self.device = Some(Device::open(&self.nodes, &frame)?);
        }
        let dev = self.device.as_mut().expect("opened above");
        let index = match dev.decode(&frame) {
            Ok(i) => i,
            Err(e) => {
                // Buffers may be stuck in the driver and the DPB no longer
                // matches it: start over from the next IRAP picture.
                self.device = None;
                self.frames.reset();
                return Err(e);
            }
        };
        if pts < self.discard_before {
            return Ok(self.reorder.push([]));
        }
        let bgra = dev.picture(index);
        Ok(self.reorder.push([DecodedFrame { pts, width: frame.width, height: frame.height, bgra }]))
    }

    fn run(mut self, commands: Receiver<Command>, results: Sender<Result<Vec<DecodedFrame>>>) {
        while let Ok(cmd) = commands.recv() {
            let result = match cmd {
                Command::Decode { data, pts } => self.decode(&data, pts),
                Command::Flush => Ok(self.reorder.drain()),
                Command::Reset { discard_before } => {
                    self.frames.reset();
                    self.reorder.drain();
                    self.discard_before = discard_before;
                    if let Some(d) = self.device.as_mut() {
                        d.release_all();
                    }
                    Ok(Vec::new())
                }
            };
            if results.send(result).is_err() {
                return;
            }
        }
    }
}

/// The `VideoDecoder` handle: forwards to the worker and waits for it.
pub struct V4l2HevcDecoder {
    commands: Sender<Command>,
    results: Receiver<Result<Vec<DecodedFrame>>>,
    is_keyframe: KeyframeCheck,
}

impl V4l2HevcDecoder {
    pub fn new(hvcc: &[u8], nal_length_size: usize, reorder_depth: usize, is_keyframe: KeyframeCheck) -> Result<Self> {
        let nodes = find_device().ok_or_else(|| anyhow!("no V4L2 stateless HEVC decoder"))?;
        let hvcc = hvcc.to_vec();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (res_tx, res_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        thread::Builder::new().name("v4l2-hevc".into()).spawn(move || {
            match HevcFrames::new(&hvcc, nal_length_size) {
                Ok(frames) => {
                    let _ = ready_tx.send(Ok(()));
                    Worker { nodes, frames, device: None, reorder: Reorder::new(reorder_depth), discard_before: i64::MIN }
                        .run(cmd_rx, res_tx);
                }
                Err(e) => drop(ready_tx.send(Err(e))),
            }
        })?;
        ready_rx.recv().map_err(|_| anyhow!("HEVC worker exited"))??;
        Ok(Self { commands: cmd_tx, results: res_rx, is_keyframe })
    }

    fn call(&self, cmd: Command) -> Result<Vec<DecodedFrame>> {
        self.commands.send(cmd).map_err(|_| anyhow!("HEVC worker exited"))?;
        self.results.recv().map_err(|_| anyhow!("HEVC worker exited"))?
    }
}

impl VideoDecoder for V4l2HevcDecoder {
    fn name(&self) -> &'static str {
        "V4L2 stateless (hardware)"
    }

    fn is_keyframe(&self, data: &[u8]) -> bool {
        (self.is_keyframe)(data)
    }

    fn decode(&mut self, data: &[u8], pts: i64) -> Result<Vec<DecodedFrame>> {
        self.call(Command::Decode { data: data.to_vec(), pts })
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        self.call(Command::Flush)
    }

    fn reset(&mut self, discard_before: i64) -> Result<()> {
        self.call(Command::Reset { discard_before }).map(drop)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn control_ids_match_the_kernel() {
        // From `v4l2-ctl --list-ctrls` on a Raspberry Pi 5.
        assert_eq!(super::CID_HEVC_SPS, 0x00a4_0a90);
        assert_eq!(super::CID_HEVC_START_CODE, 0x00a4_0a96);
        assert_eq!(super::FMT_HEVC_SLICE, u32::from_le_bytes(*b"S265"));
    }
}
