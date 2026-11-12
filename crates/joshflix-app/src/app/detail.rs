//! Title detail page: info, Play/Resume, My List, seasons and episodes.

use std::collections::HashMap;
use std::rc::Rc;

use joshflix_core::cinemeta::{Episode, MetaDetail, MetaPreview, metahub_size};
use joshflix_core::sources::Source;
use joshflix_core::store::{LibraryItem, Progress};
use joshflix_core::{MediaKind, VideoRef};
use slint::{Global, Image, Model, ModelRc, VecModel};

use super::{Controller, ImageKind, ImageSlot, Key, OpenTarget, shared};
use crate::format;
use crate::{DetailButton, DetailState, EpisodeCard, Icons, Screen};

/// Everything the player and progress tracking need about what's playing.
#[derive(Clone)]
pub struct PlayContext {
    pub name: String,
    pub poster: Option<String>,
    pub background: Option<String>,
    pub logo: Option<String>,
    pub episode_title: Option<String>,
    pub thumbnail: Option<String>,
    pub detail: Option<std::sync::Arc<MetaDetail>>,
}

/// A request to play something.
#[derive(Clone)]
pub struct PlayRequest {
    pub video: VideoRef,
    pub context: PlayContext,
    pub resume_at: Option<f64>,
    /// Re-use this source without showing the picker (Resume, next episode).
    pub preferred: Option<Source>,
    /// When the preferred source fails, open the picker instead of erroring.
    pub fallback_to_picker: bool,
}

#[derive(Clone)]
enum DetailAction {
    Play { video: VideoRef, resume: Option<f64>, last: Option<Source> },
    StartOver(VideoRef),
    ToggleList,
}

pub struct DetailCtl {
    pub kind: MediaKind,
    pub id: String,
    pub preview: Option<MetaPreview>,
    pub meta: Option<std::sync::Arc<MetaDetail>>,
    zone: i32,
    button: usize,
    actions: Vec<DetailAction>,
    seasons: Vec<u32>,
    season_focus: usize,
    episodes: Vec<Episode>,
    episode_index: usize,
    episodes_model: Rc<VecModel<EpisodeCard>>,
    progress: HashMap<String, Progress>,
    in_library: bool,
    backdrop_url: Option<String>,
    logo_url: Option<String>,
    generation: u64,
}

static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Controller {
    pub fn open_detail(&mut self, target: OpenTarget) {
        let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let detail = DetailCtl {
            kind: target.kind,
            id: target.id.clone(),
            preview: target.preview.clone(),
            meta: None,
            zone: 0,
            button: 0,
            actions: Vec::new(),
            seasons: Vec::new(),
            season_focus: 0,
            episodes: Vec::new(),
            episode_index: 0,
            episodes_model: Rc::new(VecModel::default()),
            progress: HashMap::new(),
            in_library: self.svc.store.in_library(&target.id).unwrap_or(false),
            backdrop_url: None,
            logo_url: None,
            generation,
        };
        self.detail = Some(detail);
        let state = DetailState::get(&self.ui);
        state.set_loading(true);
        state.set_status_line("".into());
        state.set_episodes(ModelRc::default());
        state.set_seasons(ModelRc::default());
        state.set_is_series(target.kind == MediaKind::Series);
        state.set_zone(0);
        state.set_button_index(0);
        state.set_buttons(ModelRc::default());
        state.set_has_logo(false);
        state.set_backdrop(Image::default());
        if let Some(preview) = &target.preview {
            self.push_preview(preview);
        } else {
            state.set_title("".into());
            state.set_meta("".into());
            state.set_description("".into());
            state.set_rating("".into());
            state.set_credits("".into());
        }
        self.set_screen(Screen::Detail);

        let cinemeta = self.svc.cinemeta.clone();
        let (kind, id) = (target.kind, target.id);
        self.spawn(async move { cinemeta.meta(kind, &id).await }, move |c, result| {
            let Some(detail) = c.detail.as_ref() else { return };
            if detail.generation != generation {
                return;
            }
            match result {
                Ok(meta) => c.detail_loaded(meta),
                Err(e) => {
                    let state = DetailState::get(&c.ui);
                    state.set_loading(false);
                    state.set_status_line(shared(format!("Couldn't load details: {e}")));
                    // Still allow playing a movie from the preview.
                    if let Some(preview) = c.detail.as_ref().and_then(|d| d.preview.clone()) {
                        c.detail_loaded(MetaDetail {
                            preview,
                            cast: vec![],
                            director: vec![],
                            status: None,
                            trailers: vec![],
                            episodes: vec![],
                        });
                    }
                }
            }
        });
    }

    fn push_preview(&mut self, preview: &MetaPreview) {
        let state = DetailState::get(&self.ui);
        state.set_title(shared(&preview.name));
        state.set_meta(shared(format::meta_line(preview)));
        state.set_description(shared(preview.description.as_deref().unwrap_or_default()));
        state.set_rating(shared(preview.imdb_rating.as_deref().unwrap_or_default()));
        let backdrop = preview.background.as_deref().map(|b| metahub_size(b, "large"));
        let logo = preview.logo.clone();
        if let Some(detail) = self.detail.as_mut() {
            detail.backdrop_url = backdrop.clone();
            detail.logo_url = logo.clone();
        }
        if let Some(url) = backdrop
            && let Some(image) = self.images.request(&url, ImageKind::Backdrop, ImageSlot::Named)
        {
            state.set_backdrop(image);
        }
        if let Some(url) = logo
            && let Some(image) = self.images.request(&url, ImageKind::Logo, ImageSlot::Named)
        {
            state.set_logo(image);
            state.set_has_logo(true);
        }
    }

    fn detail_loaded(&mut self, meta: MetaDetail) {
        let meta = std::sync::Arc::new(meta);
        self.push_preview(&meta.preview.clone());
        let state = DetailState::get(&self.ui);
        state.set_loading(false);
        let mut line = format::meta_line(&meta.preview);
        if meta.preview.kind == MediaKind::Series {
            let seasons = meta.seasons().iter().filter(|s| **s != 0).count();
            if seasons > 0 {
                line = format!("{line}  ·  {seasons} season{}", if seasons == 1 { "" } else { "s" });
            }
        }
        state.set_meta(shared(line));
        let mut credits = Vec::new();
        if !meta.cast.is_empty() {
            credits.push(format!("Starring {}", meta.cast.iter().take(4).cloned().collect::<Vec<_>>().join(", ")));
        }
        if !meta.director.is_empty() {
            credits.push(format!("Directed by {}", meta.director.join(", ")));
        }
        state.set_credits(shared(credits.join("  ·  ")));

        let Some(detail) = self.detail.as_mut() else { return };
        detail.seasons = meta.seasons();
        detail.meta = Some(meta.clone());
        detail.preview = Some(meta.preview.clone());
        let seasons: Vec<slint::SharedString> = detail
            .seasons
            .iter()
            .map(|s| if *s == 0 { "Specials".into() } else { shared(format!("Season {s}")) })
            .collect();
        state.set_seasons(ModelRc::new(VecModel::from(seasons)));
        state.set_episodes(ModelRc::from(detail.episodes_model.clone()));
        self.refresh_detail_progress();
    }

    /// Recompute buttons and episode progress (also after returning from the player).
    pub fn refresh_detail_progress(&mut self) {
        let icons = Icons::get(&self.ui);
        let Some(detail) = self.detail.as_mut() else { return };
        detail.in_library = self.svc.store.in_library(&detail.id).unwrap_or(false);
        let progress: HashMap<String, Progress> = self
            .svc
            .store
            .series_progress(&detail.id)
            .unwrap_or_default()
            .into_iter()
            .map(|p| (p.video.video_id(), p))
            .collect();
        detail.progress = progress;

        // Which video does Play target?
        let (target, label_prefix) = match detail.kind {
            MediaKind::Movie => (Some(VideoRef::movie(&detail.id)), None),
            MediaKind::Series => {
                let latest = detail.progress.values().max_by_key(|p| p.updated_at).cloned();
                let meta = detail.meta.clone();
                let pick = match (latest, meta.as_deref()) {
                    (Some(p), Some(meta)) if p.is_watched() => {
                        let (s, e) = (p.video.season.unwrap_or(1), p.video.episode.unwrap_or(1));
                        meta.next_episode(s, e).map(|n| VideoRef::episode(&detail.id, n.season, n.episode))
                    }
                    (Some(p), _) => Some(p.video.clone()),
                    (None, Some(meta)) => meta
                        .episodes
                        .iter()
                        .find(|e| e.season != 0)
                        .map(|e| VideoRef::episode(&detail.id, e.season, e.episode)),
                    (None, None) => None,
                };
                let prefix = pick.as_ref().and_then(VideoRef::episode_label).map(|l| l.replace('E', " E"));
                (pick, prefix)
            }
        };

        let mut actions = Vec::new();
        let mut buttons = Vec::new();
        if let Some(video) = target {
            let progress = detail.progress.get(&video.video_id()).cloned();
            let resume = progress.as_ref().and_then(Progress::resume_position);
            let last = progress.as_ref().and_then(|p| p.last_source.clone()).map(|l| l.stream);
            let suffix = label_prefix.map(|p| format!(" {p}")).unwrap_or_default();
            let label = match resume {
                Some(pos) => format!("Resume{suffix} · {}", format::clock(pos)),
                None => format!("Play{suffix}"),
            };
            buttons.push(DetailButton { label: shared(label), icon: icons.get_play(), primary: true });
            actions.push(DetailAction::Play { video: video.clone(), resume, last });
            if resume.is_some() {
                buttons.push(DetailButton { label: "Start over".into(), icon: icons.get_rewind(), primary: false });
                actions.push(DetailAction::StartOver(video));
            }
        }
        buttons.push(DetailButton {
            label: if detail.in_library { "In My List".into() } else { "My List".into() },
            icon: if detail.in_library { icons.get_check() } else { icons.get_plus() },
            primary: false,
        });
        actions.push(DetailAction::ToggleList);
        detail.button = detail.button.min(actions.len() - 1);
        detail.actions = actions;
        let state = DetailState::get(&self.ui);
        state.set_buttons(ModelRc::new(VecModel::from(buttons)));

        // Seasons/episodes: land on the season of the Play target the first time.
        if detail.kind == MediaKind::Series && detail.episodes.is_empty() && !detail.seasons.is_empty() {
            let target_season = detail
                .actions
                .iter()
                .find_map(|a| match a {
                    DetailAction::Play { video, .. } => video.season,
                    _ => None,
                })
                .unwrap_or(1);
            detail.season_focus = detail.seasons.iter().position(|s| *s == target_season).unwrap_or(0);
            self.load_season();
            if let Some(detail) = self.detail.as_mut()
                && let Some(DetailAction::Play { video, .. }) = detail.actions.first()
            {
                detail.episode_index = detail
                    .episodes
                    .iter()
                    .position(|e| Some(e.episode) == video.episode)
                    .unwrap_or(0);
            }
        } else {
            self.refresh_episode_cards();
        }
        self.push_detail_focus();
    }

    fn load_season(&mut self) {
        let Some(detail) = self.detail.as_mut() else { return };
        let Some(meta) = detail.meta.clone() else { return };
        let season = detail.seasons.get(detail.season_focus).copied().unwrap_or(1);
        detail.episodes = meta.episodes_in(season).cloned().collect();
        detail.episode_index = 0;
        self.refresh_episode_cards();
    }

    fn refresh_episode_cards(&mut self) {
        let Some(detail) = self.detail.as_ref() else { return };
        let today = format::today();
        let cards: Vec<EpisodeCard> = detail
            .episodes
            .iter()
            .map(|e| {
                let p = detail.progress.get(&e.id);
                let upcoming = e.released.as_deref().is_some_and(|r| r.get(..10).is_some_and(|d| d > today.as_str()));
                let aired = e.released.as_deref().and_then(format::air_date).unwrap_or_default();
                EpisodeCard {
                    label: shared(format!("E{}", e.episode)),
                    title: shared(&e.title),
                    overview: shared(e.overview.as_deref().unwrap_or_default()),
                    thumb: Image::default(),
                    has_thumb: false,
                    progress: p.map(|p| p.fraction() as f32).unwrap_or(0.0),
                    watched: p.is_some_and(Progress::is_watched),
                    aired: shared(if upcoming { format!("Coming {aired}") } else { aired }),
                    upcoming,
                }
            })
            .collect();
        let thumbs: Vec<Option<String>> = detail.episodes.iter().map(|e| e.thumbnail.clone()).collect();
        let model = detail.episodes_model.clone();
        model.set_vec(cards);
        let weak = Rc::downgrade(&model);
        for (index, url) in thumbs.into_iter().enumerate() {
            let Some(url) = url else { continue };
            let label = model.row_data(index).map(|r| r.label.to_string()).unwrap_or_default();
            if let Some(image) = self.images.request(&url, ImageKind::Thumb, ImageSlot::Episode { model: weak.clone(), index, label })
                && let Some(mut row) = model.row_data(index)
            {
                row.thumb = image;
                row.has_thumb = true;
                model.set_row_data(index, row);
            }
        }
    }

    fn push_detail_focus(&self) {
        let Some(detail) = self.detail.as_ref() else { return };
        let state = DetailState::get(&self.ui);
        state.set_zone(detail.zone);
        state.set_button_index(detail.button as i32);
        state.set_season_index(detail.season_focus as i32);
        state.set_selected_season(detail.season_focus as i32);
        state.set_episode_index(detail.episode_index as i32);
        let preview = detail
            .episodes
            .get(detail.episode_index)
            .and_then(|e| e.overview.clone())
            .unwrap_or_default();
        state.set_episode_preview(shared(preview));
    }

    pub(super) fn detail_image_ready(&mut self, url: &str, kind: ImageKind, image: &Image) {
        let Some(detail) = self.detail.as_ref() else { return };
        let state = DetailState::get(&self.ui);
        if kind == ImageKind::Backdrop && detail.backdrop_url.as_deref() == Some(url) {
            state.set_backdrop(image.clone());
        }
        if kind == ImageKind::Logo && detail.logo_url.as_deref() == Some(url) {
            state.set_logo(image.clone());
            state.set_has_logo(true);
        }
    }

    pub fn detail_key(&mut self, key: Key) -> bool {
        let Some(detail) = self.detail.as_mut() else {
            self.pop_screen();
            return true;
        };
        let is_series = detail.kind == MediaKind::Series && !detail.episodes.is_empty();
        match (detail.zone, key) {
            (_, Key::Back) => {
                self.detail = None;
                self.pop_screen();
                return true;
            }
            (0, Key::Left) => detail.button = detail.button.saturating_sub(1),
            (0, Key::Right) => detail.button = (detail.button + 1).min(detail.actions.len().saturating_sub(1)),
            (0, Key::Down) if is_series => detail.zone = 1,
            (0, Key::Ok) => {
                if let Some(action) = detail.actions.get(detail.button).cloned() {
                    self.detail_action(action);
                }
                return true;
            }
            (1, Key::Left) | (1, Key::Right) => {
                let next = if key == Key::Left {
                    detail.season_focus.checked_sub(1)
                } else {
                    (detail.season_focus + 1 < detail.seasons.len()).then_some(detail.season_focus + 1)
                };
                if let Some(next) = next {
                    detail.season_focus = next;
                    self.load_season();
                }
            }
            (1, Key::Up) => detail.zone = 0,
            (1, Key::Down) | (1, Key::Ok) => detail.zone = 2,
            (2, Key::Left) => detail.episode_index = detail.episode_index.saturating_sub(1),
            (2, Key::Right) => detail.episode_index = (detail.episode_index + 1).min(detail.episodes.len().saturating_sub(1)),
            (2, Key::Up) => detail.zone = 1,
            (2, Key::Ok) => {
                if let Some(episode) = detail.episodes.get(detail.episode_index).cloned() {
                    let video = VideoRef::episode(&detail.id, episode.season, episode.episode);
                    let progress = detail.progress.get(&episode.id).cloned();
                    let resume = progress.as_ref().and_then(Progress::resume_position);
                    let last = progress.and_then(|p| p.last_source).map(|l| l.stream);
                    self.detail_action(DetailAction::Play { video, resume, last });
                }
                return true;
            }
            _ => {}
        }
        self.push_detail_focus();
        true
    }

    fn detail_action(&mut self, action: DetailAction) {
        match action {
            DetailAction::ToggleList => self.toggle_library(),
            DetailAction::Play { video, resume, last } => {
                let Some(request) = self.play_request(video, resume, last) else { return };
                self.open_sources(request);
            }
            DetailAction::StartOver(video) => {
                let Some(request) = self.play_request(video, None, None) else { return };
                self.open_sources(request);
            }
        }
    }

    /// Build a play request for `video` from the open detail page.
    pub fn play_request(&self, video: VideoRef, resume: Option<f64>, last: Option<Source>) -> Option<PlayRequest> {
        let detail = self.detail.as_ref()?;
        let preview = detail.preview.clone()?;
        let episode = detail.meta.as_deref().and_then(|m| {
            m.episodes
                .iter()
                .find(|e| Some(e.season) == video.season && Some(e.episode) == video.episode)
                .cloned()
        });
        Some(PlayRequest {
            context: PlayContext {
                name: preview.name.clone(),
                poster: preview.poster.clone(),
                background: preview.background.clone(),
                logo: preview.logo.clone(),
                episode_title: episode.as_ref().map(|e| e.title.clone()),
                thumbnail: episode.and_then(|e| e.thumbnail),
                detail: detail.meta.clone(),
            },
            video,
            resume_at: resume,
            fallback_to_picker: last.is_some(),
            preferred: last,
        })
    }

    fn toggle_library(&mut self) {
        let Some(detail) = self.detail.as_ref() else { return };
        let Some(preview) = detail.preview.clone() else { return };
        let result = if detail.in_library {
            self.svc.store.remove_from_library(&detail.id).map(|_| "Removed from My List")
        } else {
            self.svc
                .store
                .add_to_library(LibraryItem {
                    id: preview.id.clone(),
                    kind: preview.kind,
                    name: preview.name.clone(),
                    poster: preview.poster.clone(),
                    background: preview.background.clone(),
                    logo: preview.logo.clone(),
                    release_info: preview.release_info.clone(),
                    added_at: 0,
                })
                .map(|_| "Added to My List")
        };
        match result {
            Ok(message) => self.toast(message),
            Err(e) => self.toast(&e.to_string()),
        }
        self.refresh_detail_progress();
    }
}
