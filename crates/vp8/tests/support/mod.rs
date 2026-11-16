//! Shared test support: an IVF reader, MD5, and vector loading.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use airsplice_vp8::Frame;

/// The frames of an IVF file: a 32-byte file header (the header's own
/// length is at offset 6), then per frame a 12-byte header starting with
/// the frame's size.
pub fn ivf_frames(file: &[u8]) -> Vec<&[u8]> {
    assert_eq!(&file[..4], b"DKIF", "not an IVF file");
    let mut rest = &file[usize::from(u16::from_le_bytes([file[6], file[7]]))..];
    let mut frames = Vec::new();
    while rest.len() >= 12 {
        let size = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
        let size = size.min(rest.len() - 12);
        frames.push(&rest[12..12 + size]);
        rest = &rest[12 + size..];
    }
    frames
}

pub fn vectors_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors")
}

/// Every vector present (committed ones plus any fetched by `fetch.sh`),
/// as (name, IVF bytes, expected MD5 per shown frame).
pub fn vectors() -> Vec<(String, Vec<u8>, Vec<String>)> {
    let mut names: Vec<String> = std::fs::read_dir(vectors_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".ivf"))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let ivf = std::fs::read(vectors_dir().join(&name)).unwrap();
            let md5 = std::fs::read_to_string(vectors_dir().join(format!("{name}.md5"))).unwrap();
            let md5 = md5.lines().map(|l| l.split_whitespace().next().unwrap().to_string()).collect();
            (name, ivf, md5)
        })
        .collect()
}

/// The frame as libvpx's tests hash it: the picture's Y rows, then U, then
/// V, without stride padding.
pub fn i420_bytes(frame: &Frame) -> Vec<u8> {
    let (w, h) = (frame.width as usize, frame.height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut out = Vec::with_capacity(w * h + 2 * cw * ch);
    for (plane, stride, w, h) in [(frame.y, frame.y_stride, w, h), (frame.u, frame.uv_stride, cw, ch), (frame.v, frame.uv_stride, cw, ch)] {
        for row in 0..h {
            out.extend_from_slice(&plane[row * stride..][..w]);
        }
    }
    out
}

pub fn md5_hex(data: &[u8]) -> String {
    md5(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// MD5 (RFC 1321).
pub fn md5(data: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20,
        4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    // The constants are floor(2^32 * abs(sin(i + 1))).
    let k: [u32; 64] = std::array::from_fn(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32);
    let mut state: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&(data.len() as u64 * 8).to_le_bytes());
    for chunk in message.as_chunks::<64>().0 {
        let m: [u32; 16] = std::array::from_fn(|i| u32::from_le_bytes(chunk[4 * i..4 * i + 4].try_into().unwrap()));
        let [mut a, mut b, mut c, mut d] = state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let sum = a.wrapping_add(f).wrapping_add(k[i]).wrapping_add(m[g]);
            (a, d, c) = (d, c, b);
            b = b.wrapping_add(sum.rotate_left(S[i]));
        }
        for (s, v) in state.iter_mut().zip([a, b, c, d]) {
            *s = s.wrapping_add(v);
        }
    }
    let mut out = [0; 16];
    for (o, s) in out.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        o.copy_from_slice(&s.to_le_bytes());
    }
    out
}
