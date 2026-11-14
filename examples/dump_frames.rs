//! Diagnostic: decode a file's video with the best available decoder and
//! write `pts:i64 width:u32 height:u32 bgra...` records to a file, for
//! comparing decoders across machines.
//! `cargo run --release --example dump_frames -- <input> <output>`
use std::fs::File;
use std::io::Write;

use airsplice_player::media::mp4_timing;
use airsplice_player::media::video::make_decoder;
use symphonia::core::formats::{FormatOptions, TrackType, probe::Hint};
use symphonia::core::io::MediaSourceStream;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (input, output) = (args.next().expect("input"), args.next().expect("output"));
    let mss = MediaSourceStream::new(Box::new(File::open(&input)?), Default::default());
    let mut format = symphonia::default::get_probe().probe(&Hint::new(), mss, FormatOptions::default(), Default::default())?;
    let track = format.default_track(TrackType::Video).expect("video track").clone();
    let timing = mp4_timing::read(std::path::Path::new(&input)).remove(&track.id).unwrap_or_default();
    let mut dec = make_decoder(track.codec_params.as_ref().unwrap().video().unwrap())?;
    eprintln!("decoder: {}", dec.name());
    let mut frames = Vec::new();
    while let Some(p) = format.next_packet()? {
        if p.track_id == track.id {
            frames.extend(dec.decode(&p.data, timing.pts(p.pts.get()))?);
        }
    }
    frames.extend(dec.flush()?);
    let mut out = File::create(&output)?;
    for f in &frames {
        out.write_all(&f.pts.to_le_bytes())?;
        out.write_all(&f.width.to_le_bytes())?;
        out.write_all(&f.height.to_le_bytes())?;
        out.write_all(&f.bgra)?;
    }
    eprintln!("{} frames", frames.len());
    Ok(())
}
