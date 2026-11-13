//! The start cover: one full-screen view from choosing a stream until its
//! first frame shows. It's worked out from the resolve step and the player
//! each time either changes, so it never disagrees with them.

use std::time::Duration;

use joshflix_core::cinemeta::metahub_size;
use slint::{Global, Image};

use super::detail::PlayRequest;
use super::{Controller, ImageKind, ImageSlot, shared};
use crate::{StartState, format};

/// Opening or buffering this long gets a hint that another source may help.
const SLOW_START: Duration = Duration::from_secs(15);

/// What the cover shows right now.
struct Cover<'a> {
    request: &'a PlayRequest,
    status: String,
    /// 0 get the stream, 1 open it, 2 buffer it.
    step: i32,
    /// The step's progress (0..1), or < 0 when unknown.
    progress: f32,
    hint: String,
    error: String,
}

#[derive(Default)]
pub struct StartCtl {
    /// The video whose title and art the cover shows.
    shown: Option<String>,
    backdrop_url: Option<String>,
    logo_url: Option<String>,
    /// Shown as text when there's no logo, or once the logo fails.
    title: String,
}

impl Controller {
    /// Show, update or hide the cover to match the resolve step and the player.
    pub(super) fn push_start(&mut self) {
        let state = StartState::get(&self.ui);
        let Some(cover) = self.current_cover() else {
            state.set_open(false);
            self.start.shown = None;
            return;
        };
        let id = cover.request.video.video_id();
        let request = cover.request.clone();
        state.set_status(shared(&cover.status));
        state.set_step(cover.step);
        state.set_step_progress(cover.progress);
        state.set_hint(shared(&cover.hint));
        state.set_error(shared(&cover.error));
        state.set_can_retry(true);
        drop(cover);
        if self.start.shown.as_deref() != Some(id.as_str()) {
            self.start.shown = Some(id);
            self.show_start_title(&request);
        }
        StartState::get(&self.ui).set_open(true);
    }

    fn current_cover(&self) -> Option<Cover<'_>> {
        if let Some(r) = self.sources.resolving.as_ref() {
            return Some(Cover {
                request: &r.request,
                status: r.status.clone(),
                step: 0,
                progress: r.progress,
                hint: r.hint.clone(),
                error: r.error.clone().unwrap_or_default(),
            });
        }
        let p = self.player.as_ref()?;
        if let Some(next) = p.pending_next.as_ref() {
            return Some(Cover {
                request: next,
                status: "Finding the next episode…".into(),
                step: 0,
                progress: -1.0,
                hint: String::new(),
                error: String::new(),
            });
        }
        if p.started {
            return None;
        }
        let (status, step, progress) = match (p.video_shown, p.buffering) {
            (false, _) => ("Opening the video…".to_owned(), 1, -1.0),
            (true, Some(pct)) if pct > 0 => (format!("Buffering {pct}%"), 2, pct as f32 / 100.0),
            (true, _) => ("Starting playback…".to_owned(), 2, -1.0),
        };
        let hint = if p.opened_at.elapsed() > SLOW_START {
            "This file is slow to start. Press Back to choose another source.".to_owned()
        } else {
            String::new()
        };
        Some(Cover { request: &p.request, status, step, progress, hint, error: p.error.clone().unwrap_or_default() })
    }

    /// Title, episode, resume point and art for a new request.
    fn show_start_title(&mut self, request: &PlayRequest) {
        let state = StartState::get(&self.ui);
        let (title, subtitle) = super::video_title(&request.context.name, &request.video, request.context.episode_title.as_deref());
        self.start.title = title.clone();
        state.set_subtitle(shared(subtitle));
        state.set_resume(match request.resume_at.filter(|s| *s > 1.0) {
            Some(at) => shared(format!("Resuming at {}", format::clock(at))),
            None => "".into(),
        });
        // The detail page's backdrop and logo: usually already decoded.
        let backdrop = request
            .context
            .background
            .as_deref()
            .map(|b| metahub_size(b, "large"))
            .or_else(|| request.context.thumbnail.clone());
        let logo = request.context.logo.clone();
        self.start.backdrop_url = backdrop.clone();
        self.start.logo_url = logo.clone();
        let backdrop = backdrop.and_then(|url| self.images.request(&url, ImageKind::Backdrop, ImageSlot::Named));
        state.set_has_backdrop(backdrop.is_some());
        state.set_backdrop(backdrop.unwrap_or_default());
        let logo_pending = logo.as_deref().is_some_and(|url| !self.images.has_failed(url, ImageKind::Logo));
        let logo = logo.and_then(|url| self.images.request(&url, ImageKind::Logo, ImageSlot::Named));
        // While a logo is on its way, show no title rather than flashing the plain one.
        state.set_title(if logo.is_none() && logo_pending { "".into() } else { shared(title) });
        state.set_has_logo(logo.is_some());
        state.set_logo(logo.unwrap_or_default());
    }

    /// The cover's logo won't come: show the title as text instead.
    pub(super) fn start_logo_failed(&mut self, url: &str) {
        if self.start.logo_url.as_deref() == Some(url) {
            StartState::get(&self.ui).set_title(shared(&self.start.title));
        }
    }

    pub(super) fn start_image_ready(&mut self, url: &str, kind: ImageKind, image: &Image) {
        let state = StartState::get(&self.ui);
        if kind == ImageKind::Backdrop && self.start.backdrop_url.as_deref() == Some(url) {
            state.set_backdrop(image.clone());
            state.set_has_backdrop(true);
        }
        if kind == ImageKind::Logo && self.start.logo_url.as_deref() == Some(url) {
            state.set_logo(image.clone());
            state.set_has_logo(true);
        }
    }
}
