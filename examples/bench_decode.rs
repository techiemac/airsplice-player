//! Measures video decode + colour conversion throughput:
//! `cargo run --release --example bench_decode -- <file>...`
//! Set `OUTPUT_LIMIT=1920x1080` to decode as if shown at that size.
use std::fs::File;
use std::time::Instant;

use airsplice_player::media::video::make_decoder;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;

fn main() -> anyhow::Result<()> {
    for path in std::env::args().skip(1) {
        let mss = MediaSourceStream::new(Box::new(File::open(&path)?), Default::default());
        let mut format =
            symphonia::default::get_probe().probe(&Hint::new(), mss, FormatOptions::default(), Default::default())?;
        let track = format.default_track(TrackType::Video).expect("video track").clone();
        let params = track.codec_params.as_ref().unwrap().video().unwrap();
        let (w, h) = (params.width.unwrap_or(0), params.height.unwrap_or(0));
        let mut dec = make_decoder(params)?;
        if let Some((w, h)) = std::env::var("OUTPUT_LIMIT").ok().and_then(|v| {
            let (w, h) = v.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        }) {
            dec.set_output_limit(Some((w, h)));
        }
        let tb = track.time_base.unwrap();
        let (mut frames, mut first, mut last) = (0usize, None, 0.0f64);
        let start = Instant::now();
        while let Some(p) = format.next_packet()? {
            if p.track_id != track.id {
                continue;
            }
            let secs = tb.calc_time(p.pts).map_or(0.0, |t| t.as_secs_f64());
            first.get_or_insert(secs);
            last = last.max(secs);
            frames += dec.decode(&p.data, p.pts.get())?.len();
        }
        frames += dec.flush()?.len();
        let wall = start.elapsed().as_secs_f64();
        let media = last - first.unwrap_or(0.0);
        println!(
            "{path}: {w}x{h}, {frames} frames in {wall:.2}s = {:.0} fps ({:.1}x real time)",
            frames as f64 / wall,
            media / wall
        );
    }
    Ok(())
}
