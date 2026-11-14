//! Diagnostic: `cargo run --release --example cmp_decoders -- <file>`.
//! Decodes a file with the software and hardware decoders and compares the
//! picture at each PTS, to catch frames tagged with the wrong timestamp.
use std::collections::BTreeMap;
use std::fs::File;

use airsplice_player::media::video::{make_hardware_decoder, make_software_decoder};
use symphonia::core::formats::{FormatOptions, TrackType, probe::Hint};
use symphonia::core::io::MediaSourceStream;

fn decode(path: &str, hw: bool) -> (Vec<i64>, BTreeMap<i64, Vec<u8>>) {
    let mss = MediaSourceStream::new(Box::new(File::open(path).unwrap()), Default::default());
    let mut f = symphonia::default::get_probe().probe(&Hint::new(), mss, FormatOptions::default(), Default::default()).unwrap();
    let t = f.default_track(TrackType::Video).unwrap().clone();
    let p = t.codec_params.as_ref().unwrap().video().unwrap();
    let mut d = if hw { make_hardware_decoder(p).unwrap().unwrap() } else { make_software_decoder(p).unwrap() };
    let timing = airsplice_player::media::mp4_timing::read(std::path::Path::new(path)).remove(&t.id).unwrap_or_default();
    let (mut order, mut map, mut fed) = (Vec::new(), BTreeMap::new(), Vec::new());
    while let Some(pk) = f.next_packet().unwrap() {
        if pk.track_id != t.id { continue; }
        let pts = timing.pts(pk.pts.get());
        fed.push(pts);
        for fr in d.decode(&pk.data, pts).unwrap() { order.push(fr.pts); map.insert(fr.pts, fr.bgra); }
    }
    for fr in d.flush().unwrap() { order.push(fr.pts); map.insert(fr.pts, fr.bgra); }
    if !hw { println!("fed (decode order): {:?}", &fed[..12]); }
    (order, map)
}

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let (so, sw) = decode(&path, false);
    let (ho, hw) = decode(&path, true);
    println!("software emitted: {:?}", &so[..12]);
    println!("hardware emitted: {:?}", &ho[..12]);
    let mut bad = 0;
    for (pts, a) in &sw {
        let Some(b) = hw.get(pts) else { println!("pts {pts}: missing in hardware"); continue };
        let diff: f64 = a.iter().zip(b).map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs() as f64).sum::<f64>() / a.len() as f64;
        if diff > 8.0 { bad += 1; if bad < 6 { println!("pts {pts}: mean abs diff {diff:.1}"); } }
    }
    println!("{} of {} frames differ", bad, sw.len());
}
