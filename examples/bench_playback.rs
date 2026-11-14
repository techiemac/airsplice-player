//! Plays a file through the engine (no UI) and reports frames shown versus
//! the file's frame count: `cargo run --release --example bench_playback -- <file> <secs>`
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("file");
    let secs: f64 = args.next().map_or(Ok(5.0), |s| s.parse())?;
    let player = media_play::media::Player::open(std::path::Path::new(&path), 0.0, true)?;
    let start = Instant::now();
    let (mut shown, mut max_late, mut prev_pts) = (0usize, 0.0f64, -1.0f64);
    let mut gaps = Vec::new();
    while start.elapsed().as_secs_f64() < secs && !player.is_finished() {
        if let Some(f) = player.take_frame() {
            shown += 1;
            let late = player.position() - f.pts;
            if late > 0.02 {
                eprintln!("  late {:.0} ms at frame pts {:.3}", late * 1000.0, f.pts);
            }
            max_late = max_late.max(late);
            if prev_pts >= 0.0 {
                gaps.push(f.pts - prev_pts);
            }
            prev_pts = f.pts;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let played = player.position();
    gaps.sort_by(f64::total_cmp);
    let median_gap = gaps.get(gaps.len() / 2).copied().unwrap_or(0.0);
    println!(
        "{path}: played {played:.2}s, shown {shown} frames ({:.1} fps), median frame gap {:.1} ms, max lateness {:.0} ms",
        shown as f64 / played,
        median_gap * 1000.0,
        max_late * 1000.0
    );
    Ok(())
}
