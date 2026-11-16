//! The decoder's contract beyond bit-exactness: what `decode` returns at
//! the edges of a stream, and that damaged input never panics.

mod support;

use airsplice_vp8::{Decoder, Error, is_droppable, is_keyframe};

fn vector(name: &str) -> (Vec<u8>, Vec<String>) {
    let (_, ivf, md5) = support::vectors().into_iter().find(|(n, ..)| n == name).expect("vector present");
    (ivf, md5)
}

/// Decode a frame that must be shown and return its MD5.
fn shown_md5(decoder: &mut Decoder, data: &[u8]) -> String {
    let frame = decoder.decode(data).expect("decodes").expect("is shown");
    support::md5_hex(&support::i420_bytes(&frame))
}

#[test]
fn needs_a_keyframe_first() {
    let (ivf, md5) = vector("vp80-00-comprehensive-001.ivf");
    let frames = support::ivf_frames(&ivf);
    let mut decoder = Decoder::new();
    assert!(is_keyframe(frames[0]) && !is_keyframe(frames[1]));
    assert_eq!(decoder.decode(frames[1]).err(), Some(Error::NoKeyframe));
    assert_eq!(decoder.decode(&[]).err(), Some(Error::NoKeyframe));
    assert_eq!(shown_md5(&mut decoder, frames[0]), md5[0]);
    assert_eq!(shown_md5(&mut decoder, frames[1]), md5[1]);

    decoder.reset();
    assert_eq!(decoder.decode(frames[2]).err(), Some(Error::NoKeyframe));
    assert_eq!(decoder.decode(&[]).err(), Some(Error::NoKeyframe));
    assert_eq!(shown_md5(&mut decoder, frames[0]), md5[0]);
}

#[test]
fn empty_data_repeats_the_last_shown_frame() {
    // This vector starts with a hidden keyframe, so "last shown" and "last
    // decoded" differ and at first there is nothing to repeat.
    let (ivf, md5) = vector("vp80-00-comprehensive-018.ivf");
    let frames = support::ivf_frames(&ivf);
    let mut decoder = Decoder::new();
    let mut shown = 0;
    for data in &frames {
        if let Some(frame) = decoder.decode(data).unwrap() {
            assert_eq!(support::md5_hex(&support::i420_bytes(&frame)), md5[shown]);
            shown += 1;
        }
        match decoder.decode(&[]).unwrap() {
            Some(frame) => assert_eq!(support::md5_hex(&support::i420_bytes(&frame)), md5[shown - 1]),
            None => assert_eq!(shown, 0, "only before the first shown frame"),
        }
    }
    assert_eq!(shown, frames.len() - 1, "exactly one frame is hidden");
    assert!(decoder.decode(frames[0]).unwrap().is_none(), "and it is the keyframe");
}

#[test]
fn malformed_headers_are_errors_and_change_nothing() {
    let (ivf, md5) = vector("vp80-00-comprehensive-001.ivf");
    let frames = support::ivf_frames(&ivf);
    let key = frames[0];
    let mut decoder = Decoder::new();
    assert_eq!(shown_md5(&mut decoder, key), md5[0]);
    assert_eq!(shown_md5(&mut decoder, frames[1]), md5[1]);

    let first_partition = (u32::from_le_bytes([key[0], key[1], key[2], 0]) >> 5) as usize;
    let mut zero_width = key.to_vec();
    zero_width[6..8].fill(0);
    let mut zero_height = key.to_vec();
    zero_height[8..10].fill(0);
    let mut bad_start_code = key.to_vec();
    bad_start_code[3] ^= 1;
    // 16383 x 16383 could not be described by so little data, and must not
    // be allocated.
    let mut huge = key.to_vec();
    huge[6..10].fill(0xff);
    let bad: Vec<Vec<u8>> = vec![
        zero_width,
        zero_height,
        bad_start_code,
        huge,
        key[..2].to_vec(),
        key[..9].to_vec(),
        // The first partition is cut short.
        key[..10 + first_partition - 1].to_vec(),
        key[..11].to_vec(),
        // An inter frame cut inside its first partition.
        frames[2][..4].to_vec(),
    ];
    for (i, data) in bad.iter().enumerate() {
        let err = decoder.decode(data).err().unwrap_or_else(|| panic!("case {i} decoded"));
        assert!(matches!(err, Error::Corrupt(_)), "case {i}: {err}");
        assert!(err.to_string().contains("VP8"), "case {i}: {err}");
        // Nothing moved: the last shown frame is the same, and the stream
        // carries on bit-exactly below.
        assert_eq!(shown_md5(&mut decoder, &[]), md5[1], "case {i}");
    }
    for (data, expected) in frames[2..].iter().zip(&md5[2..]) {
        assert_eq!(&shown_md5(&mut decoder, data), expected);
    }
}

#[test]
fn droppable_frames_can_be_dropped() {
    // No frame of libvpx's vectors is droppable: each updates a reference
    // or the probabilities.
    for (name, ivf, _) in support::vectors() {
        if name.starts_with("vp80-") {
            for (i, data) in support::ivf_frames(&ivf).into_iter().enumerate() {
                assert!(!is_droppable(data), "{name} frame {i}");
            }
        }
    }
    assert!(!is_droppable(&[]));
    assert!(!is_droppable(&[0x31, 0x00]));

    // The top temporal layer of this clip updates no reference: 14 frames.
    // Eight of those still update the probabilities later frames inherit,
    // so only six may be dropped. Skipping them must leave every other
    // frame's output unchanged.
    let (ivf, md5) = vector("gen-layers.ivf");
    let frames = support::ivf_frames(&ivf);
    let droppable: Vec<bool> = frames.iter().map(|f| is_droppable(f)).collect();
    assert_eq!(droppable.iter().filter(|d| **d).count(), 6, "{droppable:?}");
    assert!(!droppable[0] && !is_droppable(&frames[1][..2]));
    let mut decoder = Decoder::new();
    for ((data, expected), skip) in frames.iter().zip(&md5).zip(&droppable) {
        if !skip {
            assert_eq!(&shown_md5(&mut decoder, data), expected);
        }
    }
}

#[test]
fn decoder_can_move_between_threads() {
    fn assert_send<T: Send>() {}
    assert_send::<Decoder>();
}

/// A small deterministic generator, so the damage is the same on every run.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> usize {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as usize
    }
}

/// Feed frames, accepting any result; only a panic (or a hang, or an abort
/// on overflow in debug builds) fails.
fn feed(decoder: &mut Decoder, frames: &[Vec<u8>]) {
    for data in frames {
        if let Ok(Some(frame)) = decoder.decode(data) {
            // Touch every byte the frame promises.
            std::hint::black_box(support::i420_bytes(&frame));
        }
    }
}

/// After any damage, a keyframe must bring the decoder back to bit-exact
/// output without a reset.
fn assert_recovers(decoder: &mut Decoder, frames: &[&[u8]], md5: &[String], what: &str) {
    let mut shown = 0;
    for data in frames.iter().take(6) {
        if let Some(frame) = decoder.decode(data).unwrap_or_else(|e| panic!("{what}: {e}")) {
            assert_eq!(support::md5_hex(&support::i420_bytes(&frame)), md5[shown], "{what}: shown frame {shown}");
            shown += 1;
        }
    }
}

#[test]
fn damaged_streams_never_panic_and_recover_on_a_keyframe() {
    // VP8_FUZZ_ROUNDS multiplies the work for an occasional deeper run; the
    // damage is deterministic either way.
    let scale: usize = std::env::var("VP8_FUZZ_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let mut rng = Lcg(0x5eed);
    for (name, ivf, md5) in support::vectors() {
        let frames = support::ivf_frames(&ivf);
        // Large pictures get fewer rounds to keep the test quick.
        let rounds = scale * if ivf.len() > 150_000 { 4 } else { 12 };
        let mut decoder = Decoder::new();
        for round in 0..rounds {
            // Start at the keyframe so the damaged frames have references,
            // then damage one or two frames in a short run.
            let start = rng.next() % frames.len();
            let key = (0..=start).rev().find(|&i| is_keyframe(frames[i])).unwrap_or(0);
            let end = (start + 4).min(frames.len());
            let mut run: Vec<Vec<u8>> = frames[key.max(start.saturating_sub(3))..end].iter().map(|f| f.to_vec()).collect();
            if !is_keyframe(&run[0]) {
                run.insert(0, frames[key].to_vec());
            }
            for _ in 0..1 + round % 2 {
                let victim = rng.next() % run.len();
                let data = &mut run[victim];
                match round % 4 {
                    // Truncate anywhere, often inside the headers.
                    0 => data.truncate(rng.next() % data.len().min(if round % 8 == 0 { 40 } else { usize::MAX })),
                    // Flip bits in the frame header and first partition.
                    1 => {
                        for _ in 0..1 + rng.next() % 4 {
                            let at = rng.next() % data.len().min(64);
                            data[at] ^= 1 << (rng.next() % 8);
                        }
                    }
                    // Flip bits anywhere.
                    2 => {
                        for _ in 0..1 + rng.next() % 8 {
                            let at = rng.next() % data.len();
                            data[at] ^= 1 << (rng.next() % 8);
                        }
                    }
                    // Overwrite a stretch with noise.
                    _ => {
                        let at = rng.next() % data.len();
                        let len = (1 + rng.next() % 64).min(data.len() - at);
                        for b in &mut data[at..at + len] {
                            *b = rng.next() as u8;
                        }
                    }
                }
            }
            feed(&mut decoder, &run);
            assert_recovers(&mut decoder, &frames, &md5, &format!("{name} round {round}"));
        }
    }
}

#[test]
fn noise_never_panics() {
    let scale: usize = std::env::var("VP8_FUZZ_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let mut rng = Lcg(0xbad5eed);
    let mut decoder = Decoder::new();
    for round in 0..400 * scale {
        let len = rng.next() % 600;
        let mut data: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        if round % 2 == 0 && len >= 10 {
            // A plausible keyframe header with a small picture, so the
            // noise reaches the macroblock layers.
            data[0] &= !1;
            data[3..6].copy_from_slice(&[0x9d, 0x01, 0x2a]);
            data[6..10].copy_from_slice(&[1 + (rng.next() % 96) as u8, 0, 1 + (rng.next() % 96) as u8, 0]);
            let first_partition = (rng.next() % (len - 9)).max(1) as u32;
            data[0] = (data[0] & 0x1f) | (first_partition << 5) as u8;
            data[1] = (first_partition >> 3) as u8;
            data[2] = (first_partition >> 11) as u8;
        }
        feed(&mut decoder, &[data]);
    }
}
