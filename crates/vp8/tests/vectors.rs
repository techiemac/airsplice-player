//! Conformance: decode libvpx's VP8 test vectors and compare the MD5 of
//! every shown frame with the one libvpx publishes.

mod support;

use airsplice_vp8::Decoder;

/// Vectors committed to the repository (59 from libvpx, 8 generated here);
/// `fetch.sh` adds the rest.
const COMMITTED: usize = 67;

#[test]
fn md5_known_answers() {
    assert_eq!(support::md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(support::md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    let long = [b'a'; 1000];
    assert_eq!(support::md5_hex(&long), "cabe45dcc9ae5b66ba86600cca6b8ba8");
}

#[test]
fn libvpx_vectors_decode_bit_exactly() {
    let vectors = support::vectors();
    assert!(vectors.len() >= COMMITTED, "only {} vectors found", vectors.len());
    let mut failures = Vec::new();
    let mut frames_checked = 0;
    for (name, ivf, expected) in &vectors {
        let mut decoder = Decoder::new();
        let mut shown = 0;
        let mut failure = None;
        for (number, data) in support::ivf_frames(ivf).into_iter().enumerate() {
            match decoder.decode(data) {
                Ok(Some(frame)) => {
                    // The documented plane sizes (i420_bytes would panic on
                    // a short plane; this names the vector).
                    let (w, h) = (frame.width as usize, frame.height as usize);
                    let chroma = (h.div_ceil(2) - 1) * frame.uv_stride + w.div_ceil(2);
                    assert!(w >= 1 && h >= 1 && frame.y.len() >= (h - 1) * frame.y_stride + w, "{name}");
                    assert!(frame.u.len() >= chroma && frame.v.len() >= chroma, "{name}");
                    let got = support::md5_hex(&support::i420_bytes(&frame));
                    if expected.get(shown) != Some(&got) {
                        failure = Some(format!("{name}: frame {number} (shown frame {shown}) differs"));
                        break;
                    }
                    shown += 1;
                }
                Ok(None) => {}
                Err(e) => {
                    failure = Some(format!("{name}: frame {number}: {e}"));
                    break;
                }
            }
        }
        if failure.is_none() && shown != expected.len() {
            failure = Some(format!("{name}: {shown} frames shown, {} expected", expected.len()));
        }
        frames_checked += shown;
        failures.extend(failure);
    }
    println!("{} vectors, {frames_checked} frames checked", vectors.len());
    assert!(failures.is_empty(), "{} of {} vectors failed:\n{}", failures.len(), vectors.len(), failures.join("\n"));
}
