//! The main colors of a tiny picture, for the house lights.
//!
//! Pixels are clustered with k-means in OKLab, where distances match how
//! different colors look. Near-black and grey pixels are dropped first
//! (letterbox bars, night scenes, credits), so the clusters are the picture's
//! real colors. Clusters are ranked by area and colorfulness, and near
//! duplicates are merged. The first one is the color the picture reads as,
//! like the music server's cover color.

/// Up to this many colors per picture.
pub const MAX_COLORS: usize = 4;
/// Pixels darker than this (OKLab lightness) carry no usable color.
const MIN_LIGHTNESS: f32 = 0.2;
/// Pixels less colorful than this (OKLab chroma) count as grey.
const MIN_CHROMA: f32 = 0.04;
/// A picture needs at least this share of colorful pixels to have a palette.
const MIN_COLORFUL_SHARE: f32 = 0.04;
/// A cluster smaller than this share of the colorful pixels is a speck.
const MIN_CLUSTER_SHARE: f32 = 0.06;
/// Clusters closer than this are one color.
const MERGE_DISTANCE: f32 = 0.08;
const ITERATIONS: usize = 10;

/// A color in OKLab.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Lab {
    pub l: f32,
    pub a: f32,
    pub b: f32,
}

impl Lab {
    pub fn distance(self, other: Lab) -> f32 {
        ((self.l - other.l).powi(2) + (self.a - other.a).powi(2) + (self.b - other.b).powi(2)).sqrt()
    }

    pub fn chroma(self) -> f32 {
        self.a.hypot(self.b)
    }
}

/// One palette color and its share of the picture's colorful pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Swatch {
    pub lab: Lab,
    pub share: f32,
}

impl Swatch {
    /// Lowercase `#rrggbb`.
    pub fn hex(&self) -> String {
        hex(self.lab)
    }
}

/// Lowercase `#rrggbb`.
pub fn hex(lab: Lab) -> String {
    let [r, g, b] = to_srgb(lab);
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// The palette of packed 8-bit RGB pixels, most prominent first. Empty when
/// the picture is (nearly) all dark or grey.
pub fn extract(rgb: &[u8]) -> Vec<Swatch> {
    let total = rgb.len() / 3;
    if total == 0 {
        return Vec::new();
    }
    let pixels: Vec<Lab> = rgb
        .as_chunks::<3>()
        .0
        .iter()
        .map(|p| from_srgb(*p))
        .filter(|lab| lab.l >= MIN_LIGHTNESS && lab.chroma() >= MIN_CHROMA)
        .collect();
    if (pixels.len() as f32) < total as f32 * MIN_COLORFUL_SHARE || pixels.is_empty() {
        return Vec::new();
    }
    let clusters = kmeans(&pixels, MAX_COLORS + 1);
    let count = pixels.len() as f32;
    let mut swatches: Vec<Swatch> = clusters
        .into_iter()
        .filter(|(_, n)| *n as f32 >= count * MIN_CLUSTER_SHARE)
        .map(|(lab, n)| Swatch { lab, share: n as f32 / count })
        .collect();
    swatches.sort_by(|x, y| score(y).total_cmp(&score(x)));
    let mut palette: Vec<Swatch> = Vec::new();
    for swatch in swatches {
        match palette.iter_mut().find(|kept| kept.lab.distance(swatch.lab) < MERGE_DISTANCE) {
            Some(kept) => kept.share += swatch.share,
            None => palette.push(swatch),
        }
    }
    palette.truncate(MAX_COLORS);
    palette
}

/// Area weighted by colorfulness: a small vivid patch beats a big dull one.
fn score(swatch: &Swatch) -> f32 {
    swatch.share * (swatch.lab.chroma() + 0.05)
}

/// k-means with deterministic farthest-point seeding. Returns each centroid
/// with its pixel count.
fn kmeans(pixels: &[Lab], k: usize) -> Vec<(Lab, usize)> {
    let mut centroids: Vec<Lab> = Vec::with_capacity(k);
    // Seed with the most colorful pixel, then keep adding the pixel farthest
    // from every centroid so far.
    let first = pixels.iter().copied().max_by(|x, y| x.chroma().total_cmp(&y.chroma()));
    centroids.extend(first);
    while centroids.len() < k {
        let farthest = pixels
            .iter()
            .copied()
            .map(|p| (p, nearest(&centroids, p).1))
            .max_by(|x, y| x.1.total_cmp(&y.1));
        match farthest {
            Some((p, d)) if d > 1e-4 => centroids.push(p),
            _ => break,
        }
    }
    let mut assignment = vec![0usize; pixels.len()];
    for _ in 0..ITERATIONS {
        let mut sums = vec![(0.0f32, 0.0f32, 0.0f32, 0usize); centroids.len()];
        let mut moved = false;
        for (i, p) in pixels.iter().enumerate() {
            let (c, _) = nearest(&centroids, *p);
            moved |= assignment[i] != c;
            assignment[i] = c;
            let sum = &mut sums[c];
            sum.0 += p.l;
            sum.1 += p.a;
            sum.2 += p.b;
            sum.3 += 1;
        }
        for (centroid, (l, a, b, n)) in centroids.iter_mut().zip(&sums) {
            if *n > 0 {
                let n = *n as f32;
                *centroid = Lab { l: l / n, a: a / n, b: b / n };
            }
        }
        if !moved {
            break;
        }
    }
    let mut counts = vec![0usize; centroids.len()];
    for &c in &assignment {
        counts[c] += 1;
    }
    centroids.into_iter().zip(counts).filter(|(_, n)| *n > 0).collect()
}

fn nearest(centroids: &[Lab], p: Lab) -> (usize, f32) {
    centroids
        .iter()
        .enumerate()
        .map(|(i, c)| (i, c.distance(p)))
        .min_by(|x, y| x.1.total_cmp(&y.1))
        .unwrap_or((0, f32::INFINITY))
}

fn to_linear(c: u8) -> f32 {
    let c = f32::from(c) / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn from_linear(c: f32) -> u8 {
    let c = if c <= 0.003_130_8 { 12.92 * c } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 };
    (c.clamp(0.0, 1.0) * 255.0).round() as u8
}

pub fn from_srgb([r, g, b]: [u8; 3]) -> Lab {
    let (r, g, b) = (to_linear(r), to_linear(g), to_linear(b));
    let l = (0.412_221_47 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    Lab {
        l: 0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        a: 1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        b: 0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    }
}

pub fn to_srgb(lab: Lab) -> [u8; 3] {
    let l = (lab.l + 0.396_337_78 * lab.a + 0.215_803_76 * lab.b).powi(3);
    let m = (lab.l - 0.105_561_346 * lab.a - 0.063_854_17 * lab.b).powi(3);
    let s = (lab.l - 0.089_484_18 * lab.a - 1.291_485_5 * lab.b).powi(3);
    [
        from_linear(4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s),
        from_linear(-1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s),
        from_linear(-0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picture made of `(color, pixel count)` runs.
    fn picture(runs: &[([u8; 3], usize)]) -> Vec<u8> {
        runs.iter().flat_map(|(c, n)| std::iter::repeat_n(*c, *n).flatten()).collect()
    }

    #[test]
    fn round_trips_srgb() {
        for color in [[0, 0, 0], [255, 255, 255], [200, 116, 47], [58, 95, 138], [255, 0, 0]] {
            assert_eq!(to_srgb(from_srgb(color)), color);
        }
    }

    #[test]
    fn finds_the_main_colors_in_order() {
        let rgb = picture(&[([200, 60, 30], 600), ([40, 90, 200], 300), ([10, 10, 10], 400)]);
        let palette = extract(&rgb);
        assert_eq!(palette.len(), 2, "{palette:?}");
        assert_eq!(palette[0].hex(), "#c83c1e");
        assert_eq!(palette[1].hex(), "#285ac8");
        assert!((palette[0].share - 2.0 / 3.0).abs() < 0.01);
    }

    #[test]
    fn dark_and_grey_pictures_have_no_palette() {
        assert!(extract(&picture(&[([5, 5, 8], 1000), ([128, 128, 128], 296)])).is_empty());
        // A few colorful pixels in a dark scene are not enough.
        assert!(extract(&picture(&[([5, 5, 8], 1270), ([220, 30, 30], 26)])).is_empty());
        assert!(extract(&[]).is_empty());
    }

    #[test]
    fn specks_are_dropped_and_shades_merge() {
        let rgb = picture(&[
            ([200, 60, 30], 500),
            ([205, 64, 34], 500),
            ([30, 200, 60], 20),
        ]);
        let palette = extract(&rgb);
        assert_eq!(palette.len(), 1, "{palette:?}");
    }
}
