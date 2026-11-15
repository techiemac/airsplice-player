//! Times each available upscaler on real-world sizes, including the copy
//! to and from the GPU, which is what playback pays per frame.
//!
//!     cargo run --release --example bench_upscale

use std::time::Instant;

fn main() {
    let sizes = [(1280, 720, 2560, 1440), (1920, 1080, 3840, 2160)];
    for (name, mut up) in airsplice_player::media::upscale::all() {
        println!("{name}");
        for (w, h, ow, oh) in sizes {
            let frame: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 251) as u8).collect();
            up.upscale(&frame, w, h, ow, oh).expect("warm-up frame");
            let runs = 60;
            let started = Instant::now();
            for _ in 0..runs {
                std::hint::black_box(up.upscale(&frame, w, h, ow, oh).unwrap());
            }
            let ms = started.elapsed().as_secs_f64() * 1e3 / runs as f64;
            println!("  {w}x{h} -> {ow}x{oh}: {ms:.2} ms a frame");
        }
    }
}
