//! Image download, disk cache and downscaled decode.
//!
//! Posters and backdrops are decoded off the UI thread and resized to the size
//! they're drawn at, which keeps GPU memory low on TV chips. Raw downloads are
//! cached on disk by URL hash so cold starts don't refetch artwork.

use std::path::PathBuf;

use image::imageops::FilterType;
use sha2::{Digest, Sha256};

use crate::error::{CoreError, Result};
use crate::http::Http;

const SERVICE: &str = "the artwork server";
/// Disk cache budget; oldest files are pruned beyond this.
const DISK_BUDGET_BYTES: u64 = 300 * 1024 * 1024;

/// An RGBA8 image ready for upload.
pub struct Decoded {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone)]
pub struct ImageLoader {
    http: Http,
    cache_dir: PathBuf,
}

impl ImageLoader {
    pub fn new(http: Http, cache_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&cache_dir);
        Self { http, cache_dir }
    }

    /// Fetch (disk cache first) and decode, fitting inside `max_w`×`max_h` (aspect kept).
    pub async fn load(&self, url: &str, max_w: u32, max_h: u32) -> Result<Decoded> {
        let bytes = self.fetch(url).await?;
        tokio::task::spawn_blocking(move || decode(&bytes, max_w, max_h))
            .await
            .map_err(|e| CoreError::Invalid(e.to_string()))?
    }

    /// Fetch and decode as a card: cropped to exactly `w`×`h` (centered, like
    /// `image-fit: cover`) with corners of `radius` pixels made transparent.
    /// Baked corners let the UI draw a card as one textured quad instead of
    /// clipping it, which costs an offscreen layer per card on femtovg.
    pub async fn load_card(&self, url: &str, w: u32, h: u32, radius: f32) -> Result<Decoded> {
        let bytes = self.fetch(url).await?;
        tokio::task::spawn_blocking(move || decode_card(&bytes, w, h, radius))
            .await
            .map_err(|e| CoreError::Invalid(e.to_string()))?
    }

    /// The raw file: from the disk cache, or downloaded (and cached).
    pub async fn fetch(&self, url: &str) -> Result<Vec<u8>> {
        let path = self.cache_dir.join(cache_name(url));
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) if !bytes.is_empty() => bytes,
            _ => {
                let bytes = self.http.get_bytes(SERVICE, url).await?;
                let tmp = path.with_extension("tmp");
                if tokio::fs::write(&tmp, &bytes).await.is_ok() {
                    let _ = tokio::fs::rename(&tmp, &path).await;
                }
                bytes
            }
        };
        Ok(bytes)
    }

    /// Delete the oldest cached files beyond the budget. Call occasionally.
    pub fn prune(&self) {
        let Ok(entries) = std::fs::read_dir(&self.cache_dir) else { return };
        let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                Some((meta.modified().ok()?, meta.len(), e.path()))
            })
            .collect();
        let total: u64 = files.iter().map(|(_, len, _)| len).sum();
        if total <= DISK_BUDGET_BYTES {
            return;
        }
        files.sort_by_key(|(modified, _, _)| *modified);
        let mut excess = total - DISK_BUDGET_BYTES;
        for (_, len, path) in files {
            if excess == 0 {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                excess = excess.saturating_sub(len);
            }
        }
    }
}

fn cache_name(url: &str) -> String {
    let digest = Sha256::digest(url.as_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode and downscale. Never upscales. A source at most a third larger
/// than the target keeps its size: the GPU scales it for free, and a full
/// resize of a 1080p backdrop costs a TV CPU hundreds of milliseconds.
pub fn decode(bytes: &[u8], max_w: u32, max_h: u32) -> Result<Decoded> {
    let image = image::load_from_memory(bytes).map_err(|e| CoreError::decode(SERVICE, e))?;
    let (w, h) = (image.width(), image.height());
    let close_enough = u64::from(w) * 3 <= u64::from(max_w) * 4 && u64::from(h) * 3 <= u64::from(max_h) * 4;
    let image = if (w > max_w || h > max_h) && !close_enough {
        // Triangle is a good speed/quality balance for ~2–4x downscales.
        image.resize(max_w, max_h, FilterType::Triangle)
    } else {
        image
    };
    let rgba = image.into_rgba8();
    Ok(Decoded {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

/// Decode, crop to fill `w`×`h` (never upscaling past the source), and round the corners.
pub fn decode_card(bytes: &[u8], w: u32, h: u32, radius: f32) -> Result<Decoded> {
    let image = image::load_from_memory(bytes).map_err(|e| CoreError::decode(SERVICE, e))?;
    let (src_w, src_h) = (image.width().max(1), image.height().max(1));
    // A small source keeps its resolution; the UI scales it up.
    let shrink = (src_w as f32 / w.max(1) as f32).min(src_h as f32 / h.max(1) as f32).min(1.0);
    let (w, h) = (((w as f32 * shrink).round() as u32).max(1), ((h as f32 * shrink).round() as u32).max(1));
    let mut rgba = image.resize_to_fill(w, h, FilterType::Triangle).into_rgba8();
    round_corners(&mut rgba, radius * shrink);
    Ok(Decoded {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

/// Make everything outside a rounded rectangle transparent, antialiased over one pixel.
fn round_corners(image: &mut image::RgbaImage, radius: f32) {
    let (w, h) = image.dimensions();
    let radius = radius.min(w as f32 / 2.0).min(h as f32 / 2.0);
    if radius < 0.5 {
        return;
    }
    let span = radius.ceil() as u32;
    for dy in 0..span {
        for dx in 0..span {
            // Distance from this pixel's center to the corner circle's center.
            let (cx, cy) = (radius - (dx as f32 + 0.5), radius - (dy as f32 + 0.5));
            if cx <= 0.0 || cy <= 0.0 {
                continue;
            }
            let coverage = (radius - (cx * cx + cy * cy).sqrt() + 0.5).clamp(0.0, 1.0);
            if coverage >= 1.0 {
                continue;
            }
            for (x, y) in [(dx, dy), (w - 1 - dx, dy), (dx, h - 1 - dy), (w - 1 - dx, h - 1 - dy)] {
                let pixel = image.get_pixel_mut(x, y);
                pixel[3] = (f32::from(pixel[3]) * coverage).round() as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_and_downscales() {
        let mut png = Vec::new();
        let img = image::RgbaImage::from_pixel(400, 600, image::Rgba([10, 20, 30, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let decoded = decode(&png, 200, 200).unwrap();
        assert_eq!((decoded.width, decoded.height), (133, 200));
        assert_eq!(decoded.rgba.len(), 133 * 200 * 4);
        assert_eq!(cache_name("a").len(), 32);
    }

    #[test]
    fn close_sizes_skip_the_resize() {
        let mut png = Vec::new();
        let img = image::RgbaImage::from_pixel(1920, 1080, image::Rgba([10, 20, 30, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        // 1920 is within a third of 1520: the GPU scales it.
        assert_eq!(decode(&png, 1520, 855).map(|d| (d.width, d.height)).unwrap(), (1920, 1080));
        // Far larger than the target: downscaled.
        assert_eq!(decode(&png, 960, 540).map(|d| (d.width, d.height)).unwrap(), (960, 540));
    }

    #[test]
    fn cards_are_cropped_with_round_corners() {
        let mut png = Vec::new();
        let img = image::RgbaImage::from_pixel(400, 400, image::Rgba([10, 20, 30, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let card = decode_card(&png, 100, 150, 10.0).unwrap();
        assert_eq!((card.width, card.height), (100, 150));
        let alpha = |x: u32, y: u32| card.rgba[((y * card.width + x) * 4 + 3) as usize];
        assert_eq!(alpha(0, 0), 0);
        assert_eq!(alpha(99, 149), 0);
        assert_eq!(alpha(50, 0), 255);
        assert_eq!(alpha(50, 75), 255);
        assert!(alpha(1, 4) > 0 && alpha(1, 4) < 255, "corner edge is antialiased");
        assert_eq!(alpha(3, 3), 255);

        // A source smaller than the card keeps its pixels and the card's shape.
        let small = decode_card(&png, 800, 1200, 80.0).unwrap();
        assert_eq!((small.width, small.height), (267, 400));
    }
}
