//! HEVC decoding through a stateless V4L2 decoder (the Raspberry Pi's
//! `rpi-hevc-dec`). The bitstream logic and the decoder's output format are
//! portable and tested everywhere; only the device code is Linux-specific.

pub mod col128;
pub mod frames;
pub mod uapi;
#[cfg(target_os = "linux")]
pub mod v4l2;
