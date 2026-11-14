//! Artwork cache: decodes off-thread at the size it's drawn, then patches the
//! exact model rows (or hero/detail slots) that asked for it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Weak;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use joshflix_core::images::ImageLoader;
use slint::{Image, Model, Rgba8Pixel, SharedPixelBuffer, VecModel};
use tokio::sync::Semaphore;

use super::{Controller, with_controller};
use crate::{EpisodeCard, Poster};

/// How an image is drawn, which decides its decode size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageKind {
    Poster,
    Wide,
    Backdrop,
    Logo,
    Thumb,
}

impl ImageKind {
    /// Logical size (dp) the image is drawn at, before focus scaling.
    fn logical_size(self) -> (f32, f32) {
        match self {
            Self::Poster => (104.0 * 1.1, 156.0 * 1.1),
            Self::Wide => (196.0 * 1.1, 110.0 * 1.1),
            Self::Backdrop => (760.0, 430.0),
            Self::Logo => (320.0, 80.0),
            Self::Thumb => (172.0 * 1.1, 97.0 * 1.1),
        }
    }

    /// Corner radius (dp, at the decoded size) baked into card artwork, so the
    /// UI can draw it without a rounded clip. Matches `Theme.radius` at 1.1x.
    fn card_radius(self) -> Option<f32> {
        match self {
            Self::Poster | Self::Wide | Self::Thumb => Some(10.0 * 1.1),
            _ => None,
        }
    }
}

/// Where a finished image goes.
#[derive(Clone)]
pub enum ImageSlot {
    Poster { model: Weak<VecModel<Poster>>, index: usize, id: String },
    Episode { model: Weak<VecModel<EpisodeCard>>, index: usize, label: String },
    /// Hero, detail backdrop, logos: the controller decides if still wanted.
    Named,
}

type Pixels = SharedPixelBuffer<Rgba8Pixel>;

pub struct ImageCache {
    loader: ImageLoader,
    rt: tokio::runtime::Handle,
    scale: f32,
    cache: HashMap<String, Image>,
    order: VecDeque<String>,
    inflight: HashSet<String>,
    /// Decoded bytes held by `cache`.
    bytes: usize,
    /// Images that failed, and when: tried again after `RETRY_FAILED`
    /// (a Wi-Fi blip at startup mustn't leave blank cards for the session).
    failed: HashMap<String, std::time::Instant>,
    waiters: HashMap<String, Vec<ImageSlot>>,
    net: Arc<Semaphore>,
    net_named: Arc<Semaphore>,
    decode_cards: Arc<Semaphore>,
    decode_large: Arc<Semaphore>,
    /// Bumped when the browse hero moves on (see `backdrops_moved_on`).
    backdrop_generation: Arc<AtomicU64>,
    /// Per queued backdrop: the generation it was last asked for in.
    backdrop_wanted: HashMap<String, Arc<AtomicU64>>,
    decode_logo: Arc<Semaphore>,
    /// Decoded images waiting for their frame (see `apply_ready_images`).
    ready: VecDeque<(String, String, ImageKind, Pixels)>,
    apply_timer: slint::Timer,
}

/// Decoded artwork kept for reuse, by bytes (a backdrop is 5–8 MB, a poster 0.3 MB).
const BUDGET_BYTES: usize = 128 * 1024 * 1024;
const MAX_BACKDROPS: usize = 8;
/// About one frame at 60 Hz.
const APPLY_EVERY: std::time::Duration = std::time::Duration::from_millis(16);
const RETRY_FAILED: std::time::Duration = std::time::Duration::from_secs(60);

fn image_bytes(image: &Image) -> usize {
    let size = image.size();
    size.width as usize * size.height as usize * 4
}

/// Identifies an episode card across seasons ("E5" repeats in every season).
pub fn episode_key(card: &EpisodeCard) -> String {
    // The air date tells seasons apart when titles are generic ("Episode 5").
    format!("{}|{}|{}", card.label, card.title, card.aired)
}

fn cache_key(url: &str, kind: ImageKind, scale: f32) -> String {
    format!("{kind:?}@{scale:.2}|{url}")
}

impl ImageCache {
    pub fn new(loader: ImageLoader, rt: tokio::runtime::Handle, scale: f32) -> Self {
        Self {
            loader,
            rt,
            scale: scale.max(1.0),
            cache: HashMap::new(),
            order: VecDeque::new(),
            inflight: HashSet::new(),
            bytes: 0,
            failed: HashMap::new(),
            waiters: HashMap::new(),
            net: Arc::new(Semaphore::new(8)),
            net_named: Arc::new(Semaphore::new(2)),
            // Few concurrent decodes keep TV CPUs responsive while scrolling.
            // Backdrops and logos decode on their own permit, so cards never
            // queue behind a 1080p backdrop for a title already scrolled past.
            decode_cards: Arc::new(Semaphore::new(2)),
            decode_large: Arc::new(Semaphore::new(1)),
            backdrop_generation: Arc::new(AtomicU64::new(0)),
            backdrop_wanted: HashMap::new(),
            decode_logo: Arc::new(Semaphore::new(1)),
            ready: VecDeque::new(),
            apply_timer: slint::Timer::default(),
        }
    }

    /// The window's scale factor isn't known until it's shown, so it's refreshed.
    pub fn set_scale(&mut self, scale: f32) {
        self.scale = scale.max(1.0);
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }


    /// Ask for an image. Returns it immediately when cached; otherwise the
    /// slot is patched later and the controller is notified.
    pub fn request(&mut self, url: &str, kind: ImageKind, slot: ImageSlot) -> Option<Image> {
        if url.is_empty() {
            return None;
        }
        let key = cache_key(url, kind, self.scale);
        if let Some(image) = self.cache.get(&key) {
            let image = image.clone();
            // Least recently used goes first.
            if let Some(at) = self.order.iter().position(|k| *k == key) {
                self.order.remove(at);
                self.order.push_back(key);
            }
            return Some(image);
        }
        if let Some(at) = self.failed.get(&key) {
            if at.elapsed() < RETRY_FAILED {
                return None;
            }
            self.failed.remove(&key);
        }
        let slot_is_named = matches!(kind, ImageKind::Backdrop | ImageKind::Logo);
        self.waiters.entry(key.clone()).or_default().push(slot);
        let generation = self.backdrop_generation.load(Ordering::Relaxed);
        if !self.inflight.insert(key.clone()) {
            // Asked for again while queued: it's wanted as of now.
            if let Some(wanted) = self.backdrop_wanted.get(&key) {
                wanted.store(generation, Ordering::Relaxed);
            }
            return None;
        }
        let (w, h) = kind.logical_size();
        let (w, h) = ((w * self.scale).ceil() as u32, (h * self.scale).ceil() as u32);
        let radius = kind.card_radius().map(|r| r * self.scale);
        let loader = self.loader.clone();
        // Hero artwork has its own small download lane, so it never queues
        // behind dozens of posters.
        let net = if slot_is_named { self.net_named.clone() } else { self.net.clone() };
        // Logos get their own lane: the hero shows no title until its logo lands.
        let decode = match kind {
            ImageKind::Backdrop => self.decode_large.clone(),
            ImageKind::Logo => self.decode_logo.clone(),
            _ => self.decode_cards.clone(),
        };
        // A backdrop nobody asked for since the hero moved on is dropped
        // before it takes a download or decode slot.
        let wanted = (kind == ImageKind::Backdrop).then(|| {
            let wanted = Arc::new(AtomicU64::new(generation));
            self.backdrop_wanted.insert(key.clone(), wanted.clone());
            (self.backdrop_generation.clone(), wanted)
        });
        let stale = move || wanted.as_ref().is_some_and(|(current, at)| current.load(Ordering::Relaxed) != at.load(Ordering::Relaxed));
        let url = url.to_owned();
        self.rt.spawn(async move {
            // Err(true): no longer wanted (not a failure); Err(false): failed.
            let result: Result<Pixels, bool> = async {
                let bytes = {
                    let _permit = net.acquire().await.map_err(|_| false)?;
                    if stale() {
                        return Err(true);
                    }
                    loader.fetch(&url).await.map_err(|_| false)?
                };
                let _permit = decode.acquire().await.map_err(|_| false)?;
                if stale() {
                    return Err(true);
                }
                let started = std::time::Instant::now();
                // The pixel buffer is built on the blocking thread too, not on a runtime worker.
                let pixels = tokio::task::spawn_blocking(move || {
                    let decoded = match radius {
                        Some(radius) => joshflix_core::images::decode_card(&bytes, w, h, radius),
                        None => joshflix_core::images::decode(&bytes, w, h),
                    }
                    .ok()?;
                    Some(SharedPixelBuffer::clone_from_slice(&decoded.rgba, decoded.width, decoded.height))
                })
                .await
                .ok()
                .flatten()
                .ok_or(false)?;
                crate::perf::image_loaded(started.elapsed().as_nanos() as u64);
                Ok(pixels)
            }
            .await;
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|c| c.image_loaded(key, url, kind, result));
            });
        });
        None
    }

    /// The browse hero moved on: backdrops queued for earlier heroes are dropped
    /// when their turn to decode comes (the one asked for next is kept).
    pub fn backdrops_moved_on(&self) {
        self.backdrop_generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Start loading an image nobody shows yet, so it's ready when focus arrives.
    pub fn prefetch(&mut self, url: &str, kind: ImageKind) {
        let _ = self.request(url, kind, ImageSlot::Named);
    }

    /// This image failed to load earlier, so it won't arrive.
    pub fn has_failed(&self, url: &str, kind: ImageKind) -> bool {
        self.failed.get(&cache_key(url, kind, self.scale)).is_some_and(|at| at.elapsed() < RETRY_FAILED)
    }

    fn insert(&mut self, key: String, image: Image) {
        // Backdrops are 5–8 MB each: keep only a few, so they can't push
        // hundreds of posters out of the budget.
        if key.starts_with("Backdrop@") {
            let backdrops: Vec<String> = self.order.iter().filter(|k| k.starts_with("Backdrop@")).cloned().collect();
            for old in backdrops.iter().take(backdrops.len().saturating_sub(MAX_BACKDROPS - 1)) {
                if let Some(evicted) = self.cache.remove(old) {
                    self.bytes = self.bytes.saturating_sub(image_bytes(&evicted));
                }
                self.order.retain(|k| k != old);
            }
        }
        let size = image_bytes(&image);
        while self.bytes + size > BUDGET_BYTES
            && let Some(old) = self.order.pop_front()
        {
            if let Some(evicted) = self.cache.remove(&old) {
                self.bytes = self.bytes.saturating_sub(image_bytes(&evicted));
            }
        }
        if let Some(previous) = self.cache.insert(key.clone(), image) {
            self.bytes = self.bytes.saturating_sub(image_bytes(&previous));
            self.order.retain(|k| *k != key);
        }
        self.bytes += size;
        self.order.push_back(key);
    }


    pub fn prune_later(&self) {
        let loader = self.loader.clone();
        self.rt.spawn_blocking(move || loader.prune());
    }
}

impl Controller {
    fn image_loaded(&mut self, key: String, url: String, kind: ImageKind, pixels: Result<Pixels, bool>) {
        if let Ok(pixels) = pixels {
            // Applied a few per frame: each one is a texture upload in the
            // next render, and a burst (a row arriving, a season loading)
            // would otherwise all land in one frame. Stays in flight until then.
            self.images.ready.push_back((key, url, kind, pixels));
            if !self.images.apply_timer.running() {
                self.images.apply_timer.start(slint::TimerMode::Repeated, APPLY_EVERY, || with_controller(Controller::apply_ready_images));
            }
            return;
        }
        self.images.inflight.remove(&key);
        self.images.backdrop_wanted.remove(&key);
        let waiters = self.images.waiters.remove(&key).unwrap_or_default();
        match pixels {
            Ok(_) => {}
            // Dropped as stale: asking again later loads it.
            Err(true) => {}
            Err(false) => {
                self.images.failed.insert(key, std::time::Instant::now());
                if kind == ImageKind::Logo && waiters.iter().any(|w| matches!(w, ImageSlot::Named)) {
                    self.browse_logo_failed(&url);
                    self.start_logo_failed(&url);
                }
            }
        }
    }

    /// Hand out decoded images: up to a few cards' worth per frame, a
    /// backdrop counting as several.
    fn apply_ready_images(&mut self) {
        let mut budget = 3;
        while budget > 0 {
            let Some((key, url, kind, pixels)) = self.images.ready.pop_front() else { break };
            budget -= if kind == ImageKind::Backdrop { 3 } else { 1 };
            self.apply_image(key, url, kind, pixels);
        }
        if self.images.ready.is_empty() {
            self.images.apply_timer.stop();
        }
    }

    fn apply_image(&mut self, key: String, url: String, kind: ImageKind, pixels: Pixels) {
        self.images.inflight.remove(&key);
        self.images.backdrop_wanted.remove(&key);
        let waiters = self.images.waiters.remove(&key).unwrap_or_default();
        let image = Image::from_rgba8(pixels);
        self.images.insert(key, image.clone());
        let mut named = false;
        for slot in waiters {
            match slot {
                ImageSlot::Poster { model, index, id } => {
                    if let Some(model) = model.upgrade()
                        && let Some(mut row) = model.row_data(index)
                        && row.id.as_str() == id
                    {
                        row.image = image.clone();
                        row.has_image = true;
                        model.set_row_data(index, row);
                    }
                }
                ImageSlot::Episode { model, index, label } => {
                    if let Some(model) = model.upgrade()
                        && let Some(mut row) = model.row_data(index)
                        && episode_key(&row) == label
                    {
                        row.thumb = image.clone();
                        row.has_thumb = true;
                        model.set_row_data(index, row);
                    }
                }
                ImageSlot::Named => named = true,
            }
        }
        if named {
            self.named_image_ready(&url, kind, image);
        }
    }

    /// A hero/detail/logo image finished: apply it where it's still wanted.
    fn named_image_ready(&mut self, url: &str, kind: ImageKind, image: Image) {
        match kind {
            ImageKind::Backdrop | ImageKind::Logo => {
                if kind == ImageKind::Backdrop {
                    self.browse_backdrop_ready(url, &image);
                } else {
                    self.browse_logo_ready(url, &image);
                }
                self.detail_image_ready(url, kind, &image);
                self.start_image_ready(url, kind, &image);
            }
            _ => {}
        }
    }

    /// Load artwork for posters `range` of a poster model.
    pub fn load_posters(
        &mut self,
        model: &std::rc::Rc<VecModel<Poster>>,
        urls: &[Option<String>],
        range: std::ops::Range<usize>,
        kind: ImageKind,
    ) {
        let weak = std::rc::Rc::downgrade(model);
        for index in range {
            let Some(Some(url)) = urls.get(index) else { continue };
            let Some(mut row) = model.row_data(index) else { continue };
            if row.has_image {
                continue;
            }
            let slot = ImageSlot::Poster { model: weak.clone(), index, id: row.id.to_string() };
            if let Some(image) = self.images.request(url, kind, slot) {
                row.image = image;
                row.has_image = true;
                model.set_row_data(index, row);
            }
        }
    }
}
