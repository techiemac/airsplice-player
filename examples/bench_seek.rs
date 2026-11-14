//! Seek like the UI does and time how long playback takes to resume.
//!
//! Default: clicks on the seek bar at several positions (a seek on mouse
//! down and another on mouse up). With `drag`: a drag from 20% to 60% of
//! the file, a seek every 60 ms, then release.
//! `cargo run --release --example bench_seek -- <file> [drag]`
use std::time::{Duration, Instant};
use airsplice_player::media::Player;

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let path = std::env::args().nth(1).expect("file");
    let mut p = Player::open(std::path::Path::new(&path), 0.0, false)?;
    let d = p.info.duration.unwrap();
    let wait = |p: &Player, dur: Duration| {
        let end = Instant::now() + dur;
        while Instant::now() < end { p.take_frame(); std::thread::sleep(Duration::from_millis(5)); }
    };
    wait(&p, Duration::from_millis(1500));
    if std::env::args().nth(2).as_deref() == Some("drag") {
        // Drag from 20% to 60% over 1.5 s, a seek every 60 ms, then release.
        let start = Instant::now();
        let mut last = Instant::now() - Duration::from_secs(1);
        let mut f = 0.2;
        while start.elapsed() < Duration::from_millis(1500) {
            f = 0.2 + 0.4 * start.elapsed().as_secs_f64() / 1.5;
            if last.elapsed() >= Duration::from_millis(60) { p.seek(f * d); last = Instant::now(); }
            p.take_frame();
            std::thread::sleep(Duration::from_millis(5));
        }
        let target = f * d;
        p.seek(target);
        let t0 = Instant::now();
        while p.position() <= target + 0.2 && t0.elapsed() < Duration::from_secs(20) { p.take_frame(); std::thread::sleep(Duration::from_millis(5)); }
        println!("drag released at {target:.2}: clock moving after {} ms", t0.elapsed().as_millis());
        return Ok(());
    }
    for &frac in &[0.5, 0.2, 0.8, 0.35, 0.65, 0.1, 0.9] {
        let target = frac * d;
        p.seek(target);
        wait(&p, Duration::from_millis(3));
        p.seek(target);
        let t0 = Instant::now();
        let (mut first, mut moving) = (None, None);
        while t0.elapsed() < Duration::from_secs(20) {
            if p.take_frame().is_some() && first.is_none() { first = Some(t0.elapsed()); }
            if p.position() > target + 0.2 { moving = Some(t0.elapsed()); break; }
            std::thread::sleep(Duration::from_millis(5));
        }
        println!("click {target:6.2}: first frame {:?}, clock moving after {:?}", first.map(|t| t.as_millis()), moving.map(|t| t.as_millis()));
        wait(&p, Duration::from_millis(700));
    }
    Ok(())
}
