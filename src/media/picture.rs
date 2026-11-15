//! Still images (PNG, JPEG) and animated GIFs, played through the same
//! frame queue as video: a still is one frame with no timeline, an
//! animated GIF a clip whose frames start at their GIF delays, so it plays,
//! pauses and scrubs like a video.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use anyhow::{Context as _, Result, bail};
use image::AnimationDecoder as _;

pub const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif"];

/// Decoded frames are kept for instant scrubbing; beyond this an animation
/// is refused rather than exhausting memory.
const MAX_DECODED_BYTES: u64 = 1 << 30;

pub fn is_picture(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// What the player needs before decoding: the size, and when each frame
/// starts. One frame and no duration is a still.
#[derive(Clone, Debug, PartialEq)]
pub struct Outline {
    pub format: &'static str,
    pub width: u32,
    pub height: u32,
    pub starts: Vec<f64>,
    pub duration: Option<f64>,
}

impl Outline {
    pub fn is_still(&self) -> bool {
        self.duration.is_none()
    }
}

/// How long a GIF frame shows, from its delay in hundredths of a second.
/// Browsers show delays under 20 ms as 100 ms, and GIFs are made to rely
/// on it.
fn delay_secs(centis: u16) -> f64 {
    if centis < 2 { 0.1 } else { centis as f64 / 100.0 }
}

fn is_gif(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("gif"))
}

/// Read a picture's outline without decoding its pixels.
pub fn outline(path: &Path) -> Result<Outline> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    if is_gif(path) {
        let mut options = gif::DecodeOptions::new();
        options.skip_frame_decoding(true);
        let mut decoder = options.read_info(BufReader::new(file)).context("reading the GIF")?;
        let (width, height) = (decoder.width() as u32, decoder.height() as u32);
        let mut starts = Vec::new();
        let mut at = 0.0;
        while let Some(frame) = decoder.read_next_frame().context("reading the GIF")? {
            starts.push(at);
            at += delay_secs(frame.delay);
        }
        if starts.is_empty() {
            bail!("the GIF has no frames");
        }
        let bytes = starts.len() as u64 * width as u64 * height as u64 * 4;
        if bytes > MAX_DECODED_BYTES {
            bail!("the GIF is too large to play ({} frames at {width}x{height})", starts.len());
        }
        let duration = (starts.len() > 1).then_some(at);
        if duration.is_none() {
            starts = vec![0.0];
        }
        return Ok(Outline { format: "GIF", width, height, starts, duration });
    }
    let reader = image::ImageReader::new(BufReader::new(file)).with_guessed_format().context("reading the image")?;
    let format = match reader.format() {
        Some(image::ImageFormat::Png) => "PNG",
        Some(image::ImageFormat::Jpeg) => "JPEG",
        _ => bail!("not a PNG or JPEG image"),
    };
    let (width, height) = reader.into_dimensions().context("reading the image")?;
    Ok(Outline { format, width, height, starts: vec![0.0], duration: None })
}

fn to_bgra(mut rgba: Vec<u8>) -> Vec<u8> {
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    rgba
}

/// The frames in order, as full-size BGRA (GIF frames composited the way
/// the file says, each over what came before).
pub fn frames(path: &Path) -> Result<Box<dyn Iterator<Item = Result<Vec<u8>>>>> {
    let file = BufReader::new(File::open(path).with_context(|| format!("opening {}", path.display()))?);
    if is_gif(path) {
        let decoder = image::codecs::gif::GifDecoder::new(file).context("reading the GIF")?;
        return Ok(Box::new(
            decoder.into_frames().map(|f| f.map(|f| to_bgra(f.into_buffer().into_raw())).context("decoding a GIF frame")),
        ));
    }
    let image = image::ImageReader::new(file).with_guessed_format()?.decode().context("decoding the image")?;
    Ok(Box::new(std::iter::once(Ok(to_bgra(image.to_rgba8().into_raw())))))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Delay, Frame, Rgba, RgbaImage};
    use std::path::PathBuf;

    /// The colour of frame `i` in test animations.
    pub(crate) fn colour(i: usize) -> [u8; 4] {
        [[200, 30, 30, 255], [30, 200, 30, 255], [30, 30, 200, 255], [220, 220, 40, 255]][i % 4]
    }

    /// A 4-frame, 32x24 GIF: frames last 100, 200, 300 and 400 ms, each one
    /// solid colour.
    pub(crate) fn animated_gif(dir: &Path) -> PathBuf {
        let path = dir.join("anim.gif");
        let mut enc = GifEncoder::new(File::create(&path).unwrap());
        enc.set_repeat(Repeat::Infinite).unwrap();
        for (i, ms) in [100, 200, 300, 400].into_iter().enumerate() {
            let img = RgbaImage::from_pixel(32, 24, Rgba(colour(i)));
            enc.encode_frame(Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(ms, 1))).unwrap();
        }
        drop(enc);
        path
    }

    pub(crate) fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("airsplice-player-picture-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_animated_gif_is_a_clip_timed_by_its_delays() {
        let dir = temp_dir("outline");
        let o = outline(&animated_gif(&dir)).unwrap();
        assert_eq!((o.format, o.width, o.height), ("GIF", 32, 24));
        let close = |a: &[f64], b: &[f64]| a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9);
        assert!(close(&o.starts, &[0.0, 0.1, 0.3, 0.6]), "{:?}", o.starts);
        assert!((o.duration.unwrap() - 1.0).abs() < 1e-9);
        let frames: Vec<Vec<u8>> = frames(&animated_gif(&dir)).unwrap().map(Result::unwrap).collect();
        assert_eq!(frames.len(), 4);
        for (i, f) in frames.iter().enumerate() {
            let [r, g, b, a] = colour(i);
            assert_eq!(f.len(), 32 * 24 * 4);
            assert_eq!(&f[..4], &[b, g, r, a], "frame {i} is BGRA");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stills_have_one_frame_and_no_timeline() {
        let dir = temp_dir("stills");
        let img = RgbaImage::from_pixel(40, 30, Rgba([10, 20, 30, 255]));
        for (name, format) in [("a.png", "PNG"), ("b.jpg", "JPEG"), ("c.jpeg", "JPEG")] {
            let path = dir.join(name);
            image::DynamicImage::ImageRgba8(img.clone()).to_rgb8().save(&path).unwrap();
            let o = outline(&path).unwrap();
            assert_eq!(o, Outline { format, width: 40, height: 30, starts: vec![0.0], duration: None });
            let frames: Vec<_> = frames(&path).unwrap().map(Result::unwrap).collect();
            assert_eq!(frames.len(), 1);
            assert!(frames[0][0].abs_diff(30) <= 2 && frames[0][2].abs_diff(10) <= 2, "BGRA order: {:?}", &frames[0][..4]);
        }
        // A one-frame GIF is a still too.
        let path = dir.join("one.gif");
        let mut enc = GifEncoder::new(File::create(&path).unwrap());
        enc.encode_frame(Frame::new(img)).unwrap();
        drop(enc);
        assert!(outline(&path).unwrap().is_still());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tiny_gif_delays_play_as_browsers_play_them() {
        assert_eq!(delay_secs(0), 0.1);
        assert_eq!(delay_secs(1), 0.1);
        assert_eq!(delay_secs(2), 0.02);
        assert_eq!(delay_secs(150), 1.5);
    }

    #[test]
    fn a_file_that_is_not_a_picture_is_an_error() {
        let dir = temp_dir("bad");
        let path = dir.join("fake.png");
        std::fs::write(&path, b"not an image").unwrap();
        assert!(outline(&path).is_err());
        let path = dir.join("fake.gif");
        std::fs::write(&path, b"GIF89a broken").unwrap();
        assert!(outline(&path).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
