//! Counters for the TV log's frame summary, so a slow stretch can be matched
//! to what the app was doing (key presses, artwork arriving and its cost).

use std::sync::atomic::{AtomicU64, Ordering};

static KEYS: AtomicU64 = AtomicU64::new(0);
static IMAGES: AtomicU64 = AtomicU64::new(0);
static IMAGE_NS: AtomicU64 = AtomicU64::new(0);
static IMAGE_MAX_NS: AtomicU64 = AtomicU64::new(0);

/// A navigation key reached the UI.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn key() {
    KEYS.fetch_add(1, Ordering::Relaxed);
}

/// An image finished loading (fetch + decode, off the UI thread) in `ns`.
pub fn image_loaded(ns: u64) {
    IMAGES.fetch_add(1, Ordering::Relaxed);
    IMAGE_NS.fetch_add(ns, Ordering::Relaxed);
    IMAGE_MAX_NS.fetch_max(ns, Ordering::Relaxed);
}

/// Counts since the last call: (keys, images, mean image ms, max image ms).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn take() -> (u64, u64, f64, f64) {
    let keys = KEYS.swap(0, Ordering::Relaxed);
    let images = IMAGES.swap(0, Ordering::Relaxed);
    let total = IMAGE_NS.swap(0, Ordering::Relaxed);
    let max = IMAGE_MAX_NS.swap(0, Ordering::Relaxed);
    let mean = if images > 0 { total as f64 / images as f64 / 1e6 } else { 0.0 };
    (keys, images, mean, max as f64 / 1e6)
}
