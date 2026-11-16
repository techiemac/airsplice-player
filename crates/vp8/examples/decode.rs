//! Decode an IVF file and report speed; optionally write the frames.
//!
//!     cargo run --release -p airsplice-vp8 --example decode -- clip.ivf [out.i420] [--md5] [--loops N]
//!
//! `--md5` prints one MD5 per shown frame, comparable with libvpx's test
//! vector `.md5` files and with `ffmpeg -f framemd5`.

#[path = "../tests/support/mod.rs"]
mod support;

use std::io::Write;
use std::time::Instant;

use airsplice_vp8::Decoder;

fn main() {
    let mut args = std::env::args().skip(1);
    let mut input = None;
    let mut output = None;
    let mut md5 = false;
    let mut loops = 1;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--md5" => md5 = true,
            "--loops" => loops = args.next().and_then(|n| n.parse().ok()).expect("--loops needs a number"),
            _ if input.is_none() => input = Some(arg),
            _ => output = Some(arg),
        }
    }
    let input = input.expect("usage: decode <file.ivf> [out.i420] [--md5] [--loops N]");
    let file = std::fs::read(&input).expect("cannot read input");
    let frames = support::ivf_frames(&file);
    let mut out = output.map(|p| std::io::BufWriter::new(std::fs::File::create(p).expect("cannot create output")));

    let mut best = f64::MAX;
    let (mut shown, mut errors) = (0, 0);
    for pass in 0..loops {
        let mut decoder = Decoder::new();
        (shown, errors) = (0, 0);
        let start = Instant::now();
        for data in &frames {
            match decoder.decode(data) {
                Ok(Some(frame)) => {
                    shown += 1;
                    if pass == 0 && (md5 || out.is_some()) {
                        let bytes = support::i420_bytes(&frame);
                        if md5 {
                            println!("{}", support::md5_hex(&bytes));
                        }
                        if let Some(out) = &mut out {
                            out.write_all(&bytes).expect("write failed");
                        }
                    }
                }
                Ok(None) => {}
                Err(_) => errors += 1,
            }
        }
        best = best.min(start.elapsed().as_secs_f64());
    }
    eprintln!(
        "{} frames in, {shown} shown, {errors} errors, {:.1} frames/s (best of {loops}, including any output)",
        frames.len(),
        frames.len() as f64 / best
    );
}
