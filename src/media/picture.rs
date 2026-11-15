//! Still images (PNG, JPEG, WebP) and animated GIF and WebP, played
//! through the same frame queue as video: a still is one frame with no
//! timeline, an animation a clip whose frames start at their delays, so it
//! plays, pauses and scrubs like a video.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context as _, Result, bail};
use image::AnimationDecoder as _;

pub const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

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

/// How long an animation frame shows, from its delay in milliseconds.
/// Browsers show delays of 10 ms or less as 100 ms, and animations are
/// made to rely on it.
fn delay_secs(ms: u32) -> f64 {
    if ms <= 10 { 0.1 } else { ms as f64 / 1000.0 }
}

fn has_extension(path: &Path, ext: &str) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

fn is_gif(path: &Path) -> bool {
    has_extension(path, "gif")
}

fn is_webp(path: &Path) -> bool {
    has_extension(path, "webp")
}

/// Frame starts and size from an animation's delays, refusing ones too big
/// to keep decoded; one frame makes a still.
fn timeline(format: &'static str, width: u32, height: u32, delays_ms: &[u32]) -> Result<Outline> {
    if delays_ms.len() <= 1 {
        return Ok(Outline { format, width, height, starts: vec![0.0], duration: None });
    }
    let bytes = delays_ms.len() as u64 * width as u64 * height as u64 * 4;
    if bytes > MAX_DECODED_BYTES {
        bail!("the {format} is too large to play ({} frames at {width}x{height})", delays_ms.len());
    }
    let mut starts = Vec::with_capacity(delays_ms.len());
    let mut at = 0.0;
    for &ms in delays_ms {
        starts.push(at);
        at += delay_secs(ms);
    }
    Ok(Outline { format, width, height, starts, duration: Some(at) })
}

/// An animated WebP's canvas size and frame durations, read from its RIFF
/// chunks (VP8X and ANMF) without decoding; `None` for a still WebP.
fn webp_animation(file: &mut BufReader<File>) -> Result<Option<(u32, u32, Vec<u32>)>> {
    let u24 = |b: &[u8]| u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16;
    let mut header = [0u8; 12];
    file.read_exact(&mut header).context("reading the WebP")?;
    if &header[..4] != b"RIFF" || &header[8..] != b"WEBP" {
        bail!("not a WebP image");
    }
    let mut canvas = None;
    let mut delays = Vec::new();
    let mut chunk = [0u8; 8];
    while file.read_exact(&mut chunk).is_ok() {
        let size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]) as u64;
        let mut skip = size + (size & 1);
        match &chunk[..4] {
            b"VP8X" if size >= 10 => {
                let mut b = [0u8; 10];
                file.read_exact(&mut b).context("reading the WebP")?;
                canvas = Some((u24(&b[4..7]) + 1, u24(&b[7..10]) + 1));
                skip -= 10;
            }
            b"ANMF" if size >= 16 => {
                let mut b = [0u8; 16];
                file.read_exact(&mut b).context("reading the WebP")?;
                delays.push(u24(&b[12..15]));
                skip -= 16;
            }
            _ => {}
        }
        file.seek(SeekFrom::Current(skip as i64)).context("reading the WebP")?;
    }
    Ok(match canvas {
        Some((w, h)) if !delays.is_empty() => Some((w, h, delays)),
        _ => None,
    })
}

/// Read a picture's outline without decoding its pixels.
pub fn outline(path: &Path) -> Result<Outline> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    if is_gif(path) {
        let mut options = gif::DecodeOptions::new();
        options.skip_frame_decoding(true);
        let mut decoder = options.read_info(BufReader::new(file)).context("reading the GIF")?;
        let (width, height) = (decoder.width() as u32, decoder.height() as u32);
        let mut delays = Vec::new();
        while let Some(frame) = decoder.read_next_frame().context("reading the GIF")? {
            // GIF delays are in hundredths of a second.
            delays.push(u32::from(frame.delay) * 10);
        }
        if delays.is_empty() {
            bail!("the GIF has no frames");
        }
        return timeline("GIF", width, height, &delays);
    }
    let mut file = BufReader::new(file);
    if is_webp(path) {
        if let Some((width, height, delays)) = webp_animation(&mut file)? {
            return timeline("WebP", width, height, &delays);
        }
        file.rewind().context("reading the WebP")?;
    }
    let reader = image::ImageReader::new(file).with_guessed_format().context("reading the image")?;
    let format = match reader.format() {
        Some(image::ImageFormat::Png) => "PNG",
        Some(image::ImageFormat::Jpeg) => "JPEG",
        Some(image::ImageFormat::WebP) => "WebP",
        _ => bail!("not a PNG, JPEG or WebP image"),
    };
    let (width, height) = reader.into_dimensions().context("reading the image")?;
    Ok(Outline { format, width, height, starts: vec![0.0], duration: None })
}

/// The size a `width` x `height` picture is shown at inside `limit`
/// (physical pixels), keeping its shape; `None` when it already fits.
pub fn fit_size(width: u32, height: u32, limit: (u32, u32)) -> Option<(u32, u32)> {
    if limit.0 == 0 || limit.1 == 0 || (width <= limit.0 && height <= limit.1) {
        return None;
    }
    let scale = (limit.0 as f64 / width as f64).min(limit.1 as f64 / height as f64);
    Some((((width as f64 * scale).round() as u32).max(1), ((height as f64 * scale).round() as u32).max(1)))
}

/// A still image decoded and, if bigger than `limit`, shrunk to fit it,
/// as BGRA. The shrinking happens on the decoder's own pixel format (RGB
/// for a JPEG), so no full-size RGBA copy is ever made: the peak is the
/// decoded image alone.
pub fn still(path: &Path, limit: (u32, u32)) -> Result<(u32, u32, Vec<u8>)> {
    let file = BufReader::new(File::open(path).with_context(|| format!("opening {}", path.display()))?);
    let image = image::ImageReader::new(file).with_guessed_format()?.decode().context("decoding the image")?;
    let (width, height) = (image.width(), image.height());
    let image = match fit_size(width, height, limit) {
        Some((w, h)) => match image {
            image::DynamicImage::ImageRgb8(rgb) => image::DynamicImage::ImageRgb8(image::imageops::thumbnail(&rgb, w, h)),
            image::DynamicImage::ImageRgba8(rgba) => image::DynamicImage::ImageRgba8(image::imageops::thumbnail(&rgba, w, h)),
            other => image::DynamicImage::ImageRgba8(image::imageops::thumbnail(&other.to_rgba8(), w, h)),
        },
        None => image,
    };
    let (w, h) = (image.width(), image.height());
    Ok((w, h, to_bgra(image.into_rgba8().into_raw())))
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
    if is_webp(path) {
        let decoder = image::codecs::webp::WebPDecoder::new(file).context("reading the WebP")?;
        if decoder.has_animation() {
            return Ok(Box::new(
                decoder.into_frames().map(|f| f.map(|f| to_bgra(f.into_buffer().into_raw())).context("decoding a WebP frame")),
            ));
        }
        let file = BufReader::new(File::open(path)?);
        let image = image::ImageReader::new(file).with_guessed_format()?.decode().context("decoding the image")?;
        return Ok(Box::new(std::iter::once(Ok(to_bgra(image.to_rgba8().into_raw())))));
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

    /// A WebP still of one colour, lossless, as file bytes.
    fn webp_still(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        let img = RgbaImage::from_pixel(w, h, Rgba(rgba));
        let mut out = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut out).encode(img.as_raw(), w, h, image::ExtendedColorType::Rgba8).unwrap();
        out
    }

    /// An animated WebP built from lossless stills: each frame's VP8L chunk
    /// wrapped in an ANMF chunk with its duration.
    pub(crate) fn animated_webp(dir: &Path, durations_ms: &[u32]) -> PathBuf {
        let (w, h) = (24u32, 16u32);
        let u24 = |v: u32| v.to_le_bytes()[..3].to_vec();
        let chunk = |fourcc: &[u8], payload: &[u8]| {
            let mut c = fourcc.to_vec();
            c.extend((payload.len() as u32).to_le_bytes());
            c.extend(payload);
            if payload.len() % 2 == 1 {
                c.push(0);
            }
            c
        };
        let mut body = b"WEBP".to_vec();
        let mut vp8x = vec![0x12, 0, 0, 0]; // animation and alpha flags
        vp8x.extend(u24(w - 1));
        vp8x.extend(u24(h - 1));
        body.extend(chunk(b"VP8X", &vp8x));
        body.extend(chunk(b"ANIM", &[0, 0, 0, 0, 0, 0]));
        for (i, &ms) in durations_ms.iter().enumerate() {
            // The still's own image chunk follows its 12-byte RIFF header.
            let still = webp_still(w, h, colour(i));
            let mut anmf = [u24(0), u24(0), u24(w - 1), u24(h - 1), u24(ms)].concat();
            anmf.push(0b10); // do not blend: each frame replaces the canvas
            anmf.extend(&still[12..]);
            body.extend(chunk(b"ANMF", &anmf));
        }
        let path = dir.join("anim.webp");
        std::fs::write(&path, chunk(b"RIFF", &body)).unwrap();
        path
    }

    #[test]
    fn an_animated_webp_is_a_clip_timed_by_its_durations() {
        let dir = temp_dir("webp");
        // 0 ms plays as 100 ms, as in browsers.
        let path = animated_webp(&dir, &[50, 0, 200]);
        let o = outline(&path).unwrap();
        assert_eq!((o.format, o.width, o.height, o.starts.len()), ("WebP", 24, 16, 3));
        assert!((o.starts[1] - 0.05).abs() < 1e-9 && (o.starts[2] - 0.15).abs() < 1e-9, "{:?}", o.starts);
        assert!((o.duration.unwrap() - 0.35).abs() < 1e-9);
        let decoded: Vec<Vec<u8>> = frames(&path).unwrap().map(Result::unwrap).collect();
        assert_eq!(decoded.len(), 3);
        for (i, f) in decoded.iter().enumerate() {
            let [r, g, b, a] = colour(i);
            assert_eq!(&f[..4], &[b, g, r, a], "frame {i}");
        }

        let still = dir.join("still.webp");
        std::fs::write(&still, webp_still(10, 6, [1, 2, 3, 255])).unwrap();
        assert_eq!(outline(&still).unwrap(), Outline { format: "WebP", width: 10, height: 6, starts: vec![0.0], duration: None });
        let f: Vec<_> = frames(&still).unwrap().map(Result::unwrap).collect();
        assert_eq!((f.len(), &f[0][..4]), (1, &[3, 2, 1, 255][..]));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn big_pictures_shrink_to_fit_the_display_keeping_their_shape() {
        assert_eq!(fit_size(6708, 4471, (2400, 1300)), Some((1950, 1300)), "height-bound");
        assert_eq!(fit_size(6708, 4471, (2400, 2400)), Some((2400, 1600)), "width-bound");
        assert_eq!(fit_size(800, 600, (2400, 1300)), None, "already fits");
        assert_eq!(fit_size(6708, 4471, (0, 0)), None, "display size not known yet");
    }

    #[test]
    fn a_still_decodes_straight_to_the_size_it_is_shown() {
        let dir = temp_dir("still-fit");
        for (name, rgba) in [("rgb.jpg", false), ("rgba.png", true)] {
            let path = dir.join(name);
            let img = RgbaImage::from_pixel(1000, 500, Rgba([200, 100, 50, 255]));
            if rgba { img.save(&path).unwrap() } else { image::DynamicImage::ImageRgba8(img).to_rgb8().save(&path).unwrap() }
            let (w, h, px) = still(&path, (400, 400)).unwrap();
            assert_eq!((w, h, px.len()), (400, 200, 400 * 200 * 4), "{name}");
            assert!(px[0].abs_diff(50) <= 3 && px[2].abs_diff(200) <= 3, "{name}: BGRA order: {:?}", &px[..4]);
            assert_eq!(still(&path, (2000, 2000)).unwrap().0, 1000, "{name}: small enough already");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tiny_delays_play_as_browsers_play_them() {
        assert_eq!(delay_secs(0), 0.1);
        assert_eq!(delay_secs(10), 0.1);
        assert_eq!(delay_secs(20), 0.02);
        assert_eq!(delay_secs(1500), 1.5);
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
