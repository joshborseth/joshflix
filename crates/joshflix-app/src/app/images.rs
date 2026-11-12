//! Artwork cache: decodes off-thread at the size it's drawn, then patches the
//! exact model rows (or hero/detail slots) that asked for it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Weak;
use std::sync::Arc;

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
            Self::Poster | Self::Wide => Some(10.0 * 1.1),
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
    failed: HashSet<String>,
    waiters: HashMap<String, Vec<ImageSlot>>,
    limit: Arc<Semaphore>,
}

const CAPACITY: usize = 360;

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
            failed: HashSet::new(),
            waiters: HashMap::new(),
            // Few concurrent decodes keep TV CPUs responsive while scrolling.
            limit: Arc::new(Semaphore::new(4)),
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
            return Some(image.clone());
        }
        if self.failed.contains(&key) {
            return None;
        }
        self.waiters.entry(key.clone()).or_default().push(slot);
        if !self.inflight.insert(key.clone()) {
            return None;
        }
        let (w, h) = kind.logical_size();
        let (w, h) = ((w * self.scale).ceil() as u32, (h * self.scale).ceil() as u32);
        let radius = kind.card_radius().map(|r| r * self.scale);
        let loader = self.loader.clone();
        let limit = self.limit.clone();
        let url = url.to_owned();
        self.rt.spawn(async move {
            let result: Option<Pixels> = async {
                let _permit = limit.acquire().await.ok()?;
                let started = std::time::Instant::now();
                let decoded = match radius {
                    Some(radius) => loader.load_card(&url, w, h, radius).await.ok()?,
                    None => loader.load(&url, w, h).await.ok()?,
                };
                crate::perf::image_loaded(started.elapsed().as_nanos() as u64);
                Some(SharedPixelBuffer::clone_from_slice(&decoded.rgba, decoded.width, decoded.height))
            }
            .await;
            let _ = slint::invoke_from_event_loop(move || {
                with_controller(|c| c.image_loaded(key, url, kind, result));
            });
        });
        None
    }

    /// Start loading an image nobody shows yet, so it's ready when focus arrives.
    pub fn prefetch(&mut self, url: &str, kind: ImageKind) {
        let _ = self.request(url, kind, ImageSlot::Named);
    }

    /// This image failed to load earlier, so it won't arrive.
    pub fn has_failed(&self, url: &str, kind: ImageKind) -> bool {
        self.failed.contains(&cache_key(url, kind, self.scale))
    }

    fn insert(&mut self, key: String, image: Image) {
        if self.cache.len() >= CAPACITY
            && let Some(old) = self.order.pop_front()
        {
            self.cache.remove(&old);
        }
        self.order.push_back(key.clone());
        self.cache.insert(key, image);
    }


    pub fn prune_later(&self) {
        let loader = self.loader.clone();
        self.rt.spawn_blocking(move || loader.prune());
    }
}

impl Controller {
    fn image_loaded(&mut self, key: String, url: String, kind: ImageKind, pixels: Option<Pixels>) {
        self.images.inflight.remove(&key);
        let waiters = self.images.waiters.remove(&key).unwrap_or_default();
        let Some(pixels) = pixels else {
            self.images.failed.insert(key);
            if kind == ImageKind::Logo && waiters.iter().any(|w| matches!(w, ImageSlot::Named)) {
                self.browse_logo_failed(&url);
            }
            return;
        };
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
                        && row.label.as_str() == label
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
            ImageKind::Backdrop => {
                self.browse_backdrop_ready(url, &image);
                self.detail_image_ready(url, kind, &image);
            }
            ImageKind::Logo => {
                self.browse_logo_ready(url, &image);
                self.detail_image_ready(url, kind, &image);
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
