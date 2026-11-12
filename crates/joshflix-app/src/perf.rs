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

thread_local! {
    /// UI-thread marks for the frame being drawn, set from the rendering notifier.
    static FRAME: std::cell::RefCell<FrameMarks> = std::cell::RefCell::new(FrameMarks::default());
    /// GPU time of finished frames, in ns. Results arrive a few frames late.
    static GPU_NS: std::cell::RefCell<Vec<i64>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// What the rendering notifier saw while one frame was drawn.
#[derive(Default, Clone, Copy)]
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub struct FrameMarks {
    /// mpv drew a video frame, taking this long (ns).
    pub video_ns: Option<i64>,
    /// When the UI's own drawing started (after the video).
    draw_start_ns: Option<i64>,
    /// Slint drawing the UI and handing it to GL (ns), without the swap.
    pub draw_ns: Option<i64>,
}

/// Forget the previous frame's marks before drawing a new one.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn frame_begin() {
    FRAME.with(|f| *f.borrow_mut() = FrameMarks::default());
}

/// The marks of the frame that was just drawn.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn frame_marks() -> FrameMarks {
    FRAME.with(|f| *f.borrow())
}

/// mpv drew a video frame in `ns`.
pub fn video_drawn(ns: i64) {
    FRAME.with(|f| f.borrow_mut().video_ns = Some(ns));
}

/// The UI starts drawing over the (optional) video.
pub fn draw_started(now_ns: i64) {
    FRAME.with(|f| f.borrow_mut().draw_start_ns = Some(now_ns));
}

/// The UI finished drawing; the swap comes next.
pub fn draw_finished(now_ns: i64) {
    FRAME.with(|f| {
        let mut f = f.borrow_mut();
        f.draw_ns = f.draw_start_ns.map(|start| now_ns - start);
    });
}

/// A GPU timer query finished: the GPU spent `ns` on a frame's UI.
pub fn gpu_frame(ns: i64) {
    GPU_NS.with(|g| g.borrow_mut().push(ns));
}

/// GPU frame times since the last call.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn take_gpu() -> Vec<i64> {
    GPU_NS.with(|g| std::mem::take(&mut *g.borrow_mut()))
}

/// Monotonic ns since the first call; only differences mean anything.
pub fn now_ns() -> i64 {
    let elapsed = START.get_or_init(std::time::Instant::now).elapsed();
    i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)
}

static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
