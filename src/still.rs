//! Stills: a decoded picture as 8-bit RGB, scaled down to fit a box, and as a PNG.
//!
//! Colours follow the video's colour description: its matrix (BT.709, BT.601, BT.2020 and
//! the rest of H.273's constant-luminance-free ones) and range. A video that doesn't say gets
//! BT.709 if it's 720 lines or more and BT.601 below, as players do, and limited range.
//! Samples are converted as coded, with no change of transfer function or primaries.

use std::fs::File;
use std::path::Path;

use crate::mp4::{Video, nal_units};
use crate::params::ParamSets;
use crate::picture::{self, Picture};
use crate::{Error, Result};

/// An 8-bit RGB image, row by row, three bytes a pixel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl Rgb {
    /// The image as a PNG file.
    pub fn png(&self) -> Vec<u8> {
        black_png::encode(
            self.width,
            self.height,
            black_png::ColorType::Rgb,
            &self.pixels,
            black_png::Compression::Default,
        )
        .expect("a picture is never empty, and its pixels fit its size")
    }
}

/// The still at `seconds` into an MP4 file, as a PNG fitting in `max` by `max` pixels (0 for
/// full size): the last keyframe at or before then, or the first keyframe if there's none.
pub fn still(path: &Path, seconds: f64, max: u32) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let video = Video::read(&mut file)?;
    let config = video.hevc()?;
    let key = video
        .keyframe_at(seconds)
        .ok_or_else(|| Error::corrupt("the video has no keyframes"))?;
    let data = key.read(&mut file)?;
    let mut params = ParamSets::new();
    for nal in &config.nal_units {
        params.add(nal)?;
    }
    let pic = picture::decode(&mut params, &nal_units(&data, config.length_size)?)?;
    Ok(rgb(&pic, max).png())
}

/// The size a `width` by `height` picture is scaled to so it fits in `max` by `max`, keeping
/// its shape; never scaled up, and 0 means no limit.
pub fn fit(width: u32, height: u32, max: u32) -> (u32, u32) {
    if max == 0 || width.max(height) <= max {
        return (width, height);
    }
    let shrink = |side: u32, long: u32| {
        ((u64::from(side) * u64::from(max) + u64::from(long) / 2) / u64::from(long)).max(1) as u32
    };
    let long = width.max(height);
    (shrink(width, long), shrink(height, long))
}

/// The shown part of a picture as RGB, fitting in `max` by `max` (see [`fit`]).
pub fn rgb(pic: &Picture, max: u32) -> Rgb {
    let cropped: Vec<_> = (0..pic.planes.len()).map(|c| pic.cropped(c)).collect();
    let (w, h) = (cropped[0].0, cropped[0].1);
    let (ow, oh) = fit(w, h, max);
    let colour = pic.colour;
    // The matrix: 0 is RGB stored as G, B, R, every plane ranged like luma.
    let identity = colour.matrix == 0 && pic.chroma_format == 3;
    let range = |c: usize, depth: u8| -> (f32, f32) {
        let k = f32::from(1u16 << (depth - 8));
        let chroma = c > 0 && !identity;
        match (colour.full_range, chroma) {
            (false, false) => (16.0 * k, 219.0 * k),
            (false, true) => (128.0 * k, 224.0 * k),
            (true, false) => (0.0, ((1u32 << depth) - 1) as f32),
            (true, true) => (f32::from(1u16 << (depth - 1)), ((1u32 << depth) - 1) as f32),
        }
    };
    let mut planes = Vec::new();
    for (c, (pw, ph, samples)) in cropped.iter().enumerate() {
        let depth = if c == 0 {
            pic.bit_depth_luma
        } else {
            pic.bit_depth_chroma
        };
        let (offset, scale) = range(c, depth);
        let values: Vec<f32> = samples
            .iter()
            .map(|&s| (f32::from(s) - offset) / scale)
            .collect();
        let (sx, sy) = pic.subsampling(c);
        // Chroma sits with every other luma sample across, and between two lines down
        // (H.265 Figure E-1, chroma_sample_loc_type 0).
        let across = taps(*pw as usize, ow as usize, w, sx, 0.0);
        let down = taps(
            *ph as usize,
            oh as usize,
            h,
            sy,
            if sy > 0 { 0.5 } else { 0.0 },
        );
        planes.push(resample(&values, *pw as usize, &across, &down));
    }
    let n = (ow * oh) as usize;
    let mut pixels = Vec::with_capacity(n * 3);
    let byte = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    if let [gray] = &planes[..] {
        for &y in gray {
            pixels.extend([byte(y); 3]);
        }
    } else if identity {
        let [g, b, r] = &planes[..] else {
            unreachable!()
        };
        for ((&g, &b), &r) in g.iter().zip(b).zip(r) {
            pixels.extend([byte(r), byte(g), byte(b)]);
        }
    } else {
        let (kr, kb) = weights(colour.matrix, h);
        let kg = 1.0 - kr - kb;
        let [y, cb, cr] = &planes[..] else {
            unreachable!()
        };
        for ((&y, &cb), &cr) in y.iter().zip(cb).zip(cr) {
            let r = y + 2.0 * (1.0 - kr) * cr;
            let b = y + 2.0 * (1.0 - kb) * cb;
            let g = (y - kr * r - kb * b) / kg;
            pixels.extend([byte(r), byte(g), byte(b)]);
        }
    }
    Rgb {
        width: ow,
        height: oh,
        pixels,
    }
}

/// Kr and Kb for a matrix_coefficients value (H.273 Table 4), for a picture `height` lines
/// high when it's unspecified.
fn weights(matrix: u8, height: u32) -> (f32, f32) {
    match matrix {
        1 => (0.2126, 0.0722),
        4 => (0.30, 0.11),
        5 | 6 => (0.299, 0.114),
        7 => (0.212, 0.087),
        9 | 10 => (0.2627, 0.0593),
        _ if height >= 720 => (0.2126, 0.0722),
        _ => (0.299, 0.114),
    }
}

/// For each of `m` output samples along a line, where its input samples start and their
/// weights: a tent filter as wide as an output sample (but at least two input samples, so
/// scaling up interpolates). `n` input samples span `luma` luma samples, `1 << sub` each,
/// the first centred `offset` luma samples in.
fn taps(n: usize, m: usize, luma: u32, sub: u32, offset: f64) -> Vec<(usize, Vec<f32>)> {
    let step = f64::from(luma) / m as f64;
    let size = f64::from(1u32 << sub);
    let radius = (step / size).max(1.0);
    (0..m)
        .map(|o| {
            let centre = (((o as f64 + 0.5) * step - 0.5) - offset) / size;
            let lo = (centre - radius).floor() as isize + 1;
            let hi = (centre + radius).ceil() as isize - 1;
            let clamp = |i: isize| i.clamp(0, n as isize - 1) as usize;
            let start = clamp(lo);
            let mut weights = vec![0.0; clamp(hi) - start + 1];
            for i in lo..=hi {
                weights[clamp(i) - start] += (1.0 - (i as f64 - centre).abs() / radius).max(0.0);
            }
            let total: f64 = weights.iter().sum();
            (start, weights.iter().map(|w| (w / total) as f32).collect())
        })
        .collect()
}

/// Resample a plane `width` wide: across, then down.
fn resample(
    plane: &[f32],
    width: usize,
    across: &[(usize, Vec<f32>)],
    down: &[(usize, Vec<f32>)],
) -> Vec<f32> {
    let dot = |values: &mut dyn Iterator<Item = f32>, weights: &[f32]| -> f32 {
        values.zip(weights).map(|(v, w)| v * w).sum()
    };
    let ow = across.len();
    let rows: Vec<f32> = plane
        .chunks_exact(width)
        .flat_map(|row| {
            across
                .iter()
                .map(|(start, w)| dot(&mut row[*start..].iter().copied(), w))
        })
        .collect();
    let mut out = Vec::with_capacity(ow * down.len());
    for (start, w) in down {
        for x in 0..ow {
            out.push(dot(
                &mut rows[start * ow + x..].iter().step_by(ow).copied(),
                w,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::Colour;

    fn picture(chroma_format: u8, colour: Colour, y: u16, cb: u16, cr: u16) -> Picture {
        let (w, h) = (16, 8);
        let mut planes = vec![vec![y; w * h]];
        let c = match chroma_format {
            0 => 0,
            1 => w * h / 4,
            2 => w * h / 2,
            _ => w * h,
        };
        if c > 0 {
            planes.push(vec![cb; c]);
            planes.push(vec![cr; c]);
        }
        Picture {
            width: w as u32,
            height: h as u32,
            chroma_format,
            bit_depth_luma: 8,
            bit_depth_chroma: 8,
            crop: [0; 4],
            colour,
            planes,
        }
    }

    #[test]
    fn sizes_fit_the_box() {
        assert_eq!(fit(1920, 1080, 320), (320, 180));
        assert_eq!(fit(1080, 1920, 320), (180, 320));
        assert_eq!(fit(720, 480, 320), (320, 213));
        assert_eq!(fit(200, 120, 320), (200, 120));
        assert_eq!(fit(1920, 1080, 0), (1920, 1080));
        assert_eq!(fit(4000, 1, 100), (100, 1));
    }

    #[test]
    fn taps_sum_to_one_and_stay_inside() {
        for (n, m, luma, sub) in [
            (1920, 320, 1920, 0),
            (960, 320, 1920, 1),
            (100, 100, 200, 1),
            (7, 3, 7, 0),
        ] {
            for (start, w) in taps(n, m, luma, sub, 0.5) {
                assert!(start + w.len() <= n);
                assert!((w.iter().sum::<f32>() - 1.0).abs() < 1e-5);
            }
        }
    }

    #[test]
    fn known_colours() {
        let limited = Colour::default();
        let full = Colour {
            full_range: true,
            matrix: 1,
            ..limited
        };
        let px = |p: &Picture| rgb(p, 0).pixels[..3].to_vec();
        // Within one: the coded red is itself rounded.
        let near = |a: Vec<u8>, b: [u8; 3]| a.iter().zip(b).all(|(&a, b)| a.abs_diff(b) <= 1);
        // Limited-range white, black and BT.709 red (from its definition).
        assert_eq!(px(&picture(1, limited, 235, 128, 128)), [255, 255, 255]);
        assert_eq!(px(&picture(1, limited, 16, 128, 128)), [0, 0, 0]);
        let bt709 = Colour {
            matrix: 1,
            ..limited
        };
        assert!(near(px(&picture(1, bt709, 63, 102, 240)), [255, 0, 0]));
        assert_eq!(px(&picture(3, full, 128, 128, 128)), [128, 128, 128]);
        assert_eq!(px(&picture(0, full, 200, 0, 0)), [200, 200, 200]);
        // Small and unspecified: BT.601 red.
        assert!(near(px(&picture(2, limited, 81, 90, 240)), [255, 0, 0]));
        // GBR.
        let gbr = Colour {
            matrix: 0,
            full_range: true,
            ..limited
        };
        assert_eq!(px(&picture(3, gbr, 10, 20, 30)), [30, 10, 20]);
    }
}
