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
        tokio::task::spawn_blocking(move || decode(&bytes, max_w, max_h))
            .await
            .map_err(|e| CoreError::Invalid(e.to_string()))?
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

/// Decode and downscale. Never upscales.
pub fn decode(bytes: &[u8], max_w: u32, max_h: u32) -> Result<Decoded> {
    let image = image::load_from_memory(bytes).map_err(|e| CoreError::decode(SERVICE, e))?;
    let (w, h) = (image.width(), image.height());
    let image = if w > max_w || h > max_h {
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
}
