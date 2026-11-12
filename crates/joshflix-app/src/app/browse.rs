//! Home, Movies and Shows: a hero for the focused title over rows of posters.

use std::rc::Rc;

use joshflix_core::MediaKind;
use joshflix_core::cinemeta::{CatalogId, CatalogQuery, MetaPreview, metahub_size};
use joshflix_core::store::Progress;
use slint::{Global, Image, Model, ModelRc, VecModel};

use super::{Controller, ImageKind, ImageSlot, Key, OpenTarget, Section, poster, shared};
use crate::format;
use crate::{BrowseState, Hero, Poster, Row};

/// Everything shown in the hero for a focused card.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HeroInfo {
    pub title: String,
    pub logo: Option<String>,
    pub backdrop: Option<String>,
    pub meta: String,
    pub rating: String,
    pub description: String,
}

impl HeroInfo {
    pub fn from_meta(meta: &MetaPreview) -> Self {
        Self {
            title: meta.name.clone(),
            logo: meta.logo.clone(),
            backdrop: meta
                .background
                .as_deref()
                .map(|b| metahub_size(b, "medium")),
            meta: format::meta_line(meta),
            rating: meta.imdb_rating.clone().unwrap_or_default(),
            description: meta.description.clone().unwrap_or_default(),
        }
    }
}

/// One card's data.
#[derive(Debug, Clone)]
pub struct RowItem {
    pub id: String,
    pub kind: MediaKind,
    pub title: String,
    pub subtitle: String,
    pub image: Option<String>,
    pub progress: f32,
    pub badge: String,
    pub hero: HeroInfo,
    pub target: OpenTarget,
}

impl RowItem {
    pub fn from_meta(meta: &MetaPreview) -> Self {
        Self {
            id: meta.id.clone(),
            kind: meta.kind,
            title: meta.name.clone(),
            subtitle: meta.release_info.clone().unwrap_or_default(),
            image: meta.poster.as_deref().map(|p| metahub_size(p, "small")),
            progress: 0.0,
            badge: String::new(),
            hero: HeroInfo::from_meta(meta),
            target: OpenTarget { kind: meta.kind, id: meta.id.clone(), preview: Some(meta.clone()), focus_resume: false },
        }
    }

    /// Continue Watching card.
    pub fn from_progress(p: &Progress) -> Self {
        let left = format::time_left(p.duration_secs - p.position_secs);
        let subtitle = match (p.video.episode_label(), &p.episode_title, p.up_next) {
            (Some(label), Some(title), true) => format!("Up next · {label} · {title}"),
            (Some(label), _, true) => format!("Up next · {label}"),
            (Some(label), Some(title), false) => format!("{label} · {title} · {left}"),
            (Some(label), None, false) => format!("{label} · {left}"),
            (None, _, _) => left,
        };
        Self {
            id: p.video.meta_id.clone(),
            kind: p.video.kind,
            title: p.name.clone(),
            subtitle,
            image: p.thumbnail.clone().or_else(|| p.background.clone()).or_else(|| p.poster.clone()),
            progress: if p.up_next { 0.0 } else { p.fraction() as f32 },
            badge: if p.up_next { "UP NEXT".into() } else { String::new() },
            hero: HeroInfo {
                title: p.name.clone(),
                logo: p.logo.clone(),
                backdrop: p.background.clone(),
                meta: match (&p.video.episode_label(), &p.episode_title) {
                    (Some(label), Some(title)) => format!("{label}  ·  {title}"),
                    (Some(label), None) => label.clone(),
                    _ => format::time_left(p.duration_secs - p.position_secs),
                },
                rating: String::new(),
                description: if p.up_next { "Ready for the next episode.".into() } else { format!("Resume at {}", format::clock(p.position_secs)) },
            },
            target: OpenTarget { kind: p.video.kind, id: p.video.meta_id.clone(), preview: None, focus_resume: true },
        }
    }

    pub fn to_poster(&self) -> Poster {
        let mut p = poster(&self.id, self.kind, &self.title, &self.subtitle);
        p.progress = self.progress;
        p.badge = shared(&self.badge);
        p
    }
}

#[derive(Debug, Clone)]
pub enum RowSource {
    Catalog(CatalogQuery),
    ContinueWatching,
    MyList,
}

pub struct RowCtl {
    pub title: String,
    pub source: RowSource,
    pub items: Vec<RowItem>,
    pub model: Rc<VecModel<Poster>>,
    pub focused: usize,
    pub loading: bool,
    pub wide: bool,
    pub empty_text: String,
}

impl RowCtl {
    pub fn new(title: impl Into<String>, source: RowSource) -> Self {
        let wide = matches!(source, RowSource::ContinueWatching);
        Self {
            title: title.into(),
            source,
            items: Vec::new(),
            model: Rc::new(VecModel::default()),
            focused: 0,
            loading: true,
            wide,
            empty_text: String::new(),
        }
    }

    pub fn set_items(&mut self, items: Vec<RowItem>) {
        self.model.set_vec(items.iter().map(RowItem::to_poster).collect::<Vec<_>>());
        self.items = items;
        self.focused = self.focused.min(self.items.len().saturating_sub(1));
        self.loading = false;
    }

    pub fn to_row(&self) -> Row {
        Row {
            title: shared(&self.title),
            items: ModelRc::from(self.model.clone()),
            focused_col: self.focused as i32,
            wide: self.wide,
            loading: self.loading,
            empty_text: shared(&self.empty_text),
            hidden: self.is_hidden(),
        }
    }

    pub fn image_urls(&self) -> Vec<Option<String>> {
        self.items.iter().map(|i| i.image.clone()).collect()
    }

    pub fn image_kind(&self) -> ImageKind {
        if self.wide { ImageKind::Wide } else { ImageKind::Poster }
    }

    /// Local rows (Continue Watching, My List) collapse when empty.
    pub fn is_hidden(&self) -> bool {
        !self.loading && self.items.is_empty() && !matches!(self.source, RowSource::Catalog(_))
    }

    pub fn height(&self) -> f32 {
        if self.is_hidden() {
            return 0.0;
        }
        // Matches PosterRow: 26 title + card + 18 gap.
        if self.wide { 26.0 + 110.0 + 34.0 + 18.0 } else { 26.0 + 156.0 + 18.0 }
    }

    pub fn is_navigable(&self) -> bool {
        !self.items.is_empty()
    }
}

pub struct BrowseCtl {
    pub section: Section,
    pub rows: Vec<RowCtl>,
    pub row: usize,
    pub model: Rc<VecModel<Row>>,
    pub loaded: bool,
    pub generation: u64,
    pub hero_backdrop: Option<String>,
    pub hero_logo: Option<String>,
    pub shown_backdrop: Option<String>,
    pub show_b: bool,
    /// Until the user moves, keep focus on the first row with content.
    pub user_moved: bool,
}

impl BrowseCtl {
    pub fn new(section: Section) -> Self {
        Self {
            section,
            rows: Vec::new(),
            row: 0,
            model: Rc::new(VecModel::default()),
            loaded: false,
            generation: 0,
            hero_backdrop: None,
            hero_logo: None,
            shown_backdrop: None,
            show_b: false,
            user_moved: false,
        }
    }

    fn focused_item(&self) -> Option<&RowItem> {
        let row = self.rows.get(self.row)?;
        row.items.get(row.focused)
    }
}

/// Row plan for each section.
fn plan(section: Section) -> Vec<RowCtl> {
    let year = format::current_year().to_string();
    let q = |kind, catalog| CatalogQuery::new(kind, catalog);
    let mut rows = Vec::new();
    match section {
        Section::Home => {
            rows.push(RowCtl::new("Continue Watching", RowSource::ContinueWatching));
            rows.push(RowCtl::new("My List", RowSource::MyList));
            rows.push(RowCtl::new("Popular Movies", RowSource::Catalog(q(MediaKind::Movie, CatalogId::Top))));
            rows.push(RowCtl::new("Popular Shows", RowSource::Catalog(q(MediaKind::Series, CatalogId::Top))));
            rows.push(RowCtl::new("Top Rated Movies", RowSource::Catalog(q(MediaKind::Movie, CatalogId::ImdbRating))));
            rows.push(RowCtl::new("Top Rated Shows", RowSource::Catalog(q(MediaKind::Series, CatalogId::ImdbRating))));
            rows.push(RowCtl::new(format!("New in {year}: Movies"), RowSource::Catalog(q(MediaKind::Movie, CatalogId::Year).genre(&year))));
            rows.push(RowCtl::new(format!("New in {year}: Shows"), RowSource::Catalog(q(MediaKind::Series, CatalogId::Year).genre(&year))));
            for (genre, title, kind) in [
                ("Action", "Action Movies", MediaKind::Movie),
                ("Comedy", "Comedy Shows", MediaKind::Series),
                ("Sci-Fi", "Sci-Fi Movies", MediaKind::Movie),
                ("Animation", "Animated Movies", MediaKind::Movie),
            ] {
                rows.push(RowCtl::new(title, RowSource::Catalog(q(kind, CatalogId::Top).genre(genre))));
            }
        }
        Section::Movies | Section::Shows => {
            let (kind, noun) = if section == Section::Movies { (MediaKind::Movie, "Movies") } else { (MediaKind::Series, "Shows") };
            rows.push(RowCtl::new(format!("Popular {noun}"), RowSource::Catalog(q(kind, CatalogId::Top))));
            rows.push(RowCtl::new(format!("Top Rated {noun}"), RowSource::Catalog(q(kind, CatalogId::ImdbRating))));
            rows.push(RowCtl::new(format!("New in {year}"), RowSource::Catalog(q(kind, CatalogId::Year).genre(&year))));
            for genre in ["Action", "Comedy", "Drama", "Sci-Fi", "Thriller", "Crime", "Animation", "Horror", "Documentary", "Romance", "Family", "Fantasy"] {
                rows.push(RowCtl::new(format!("{genre}"), RowSource::Catalog(q(kind, CatalogId::Top).genre(genre))));
            }
        }
        _ => {}
    }
    rows
}

impl Controller {
    fn browse_index(&self, section: Section) -> usize {
        self.browse.iter().position(|b| b.section == section).unwrap_or(0)
    }

    pub fn show_browse(&mut self, section: Section) {
        let index = self.browse_index(section);
        if !self.browse[index].loaded {
            self.browse[index].loaded = true;
            self.browse[index].rows = plan(section);
            self.browse[index].generation += 1;
            let generation = self.browse[index].generation;
            for row in 0..self.browse[index].rows.len() {
                self.load_row(index, row, generation);
            }
        } else {
            // Local rows can change while away (progress, My List).
            for row in 0..self.browse[index].rows.len() {
                if !matches!(self.browse[index].rows[row].source, RowSource::Catalog(_)) {
                    let generation = self.browse[index].generation;
                    self.load_row(index, row, generation);
                }
            }
        }
        // Start on the first non-empty row.
        let b = &mut self.browse[index];
        if !b.rows.get(b.row).is_some_and(|r| r.is_navigable() || r.loading) {
            b.row = b.rows.iter().position(|r| r.is_navigable()).unwrap_or(0);
        }
        let rows: Vec<Row> = b.rows.iter().map(RowCtl::to_row).collect();
        b.model.set_vec(rows);
        let state = BrowseState::get(&self.ui);
        state.set_rows(ModelRc::from(b.model.clone()));
        state.set_section_title(match section {
            Section::Movies => "MOVIES".into(),
            Section::Shows => "SHOWS".into(),
            _ => "".into(),
        });
        self.push_browse_focus(index);
        self.load_visible_rows(index);
    }

    fn load_row(&mut self, browse: usize, row: usize, generation: u64) {
        let source = self.browse[browse].rows[row].source.clone();
        match source {
            RowSource::ContinueWatching => {
                let items: Vec<RowItem> = self
                    .svc
                    .store
                    .continue_watching(20)
                    .unwrap_or_default()
                    .iter()
                    .map(RowItem::from_progress)
                    .collect();
                self.fill_row(browse, row, items);
            }
            RowSource::MyList => {
                let items: Vec<RowItem> = self
                    .svc
                    .store
                    .library()
                    .unwrap_or_default()
                    .iter()
                    .map(|item| {
                        let meta = MetaPreview {
                            id: item.id.clone(),
                            kind: item.kind,
                            name: item.name.clone(),
                            poster: item.poster.clone(),
                            background: item.background.clone(),
                            logo: item.logo.clone(),
                            description: None,
                            release_info: item.release_info.clone(),
                            imdb_rating: None,
                            genres: Vec::new(),
                            runtime: None,
                        };
                        RowItem::from_meta(&meta)
                    })
                    .collect();
                self.fill_row(browse, row, items);
            }
            RowSource::Catalog(query) => {
                let cinemeta = self.svc.cinemeta.clone();
                self.spawn(async move { cinemeta.catalog(&query).await }, move |c, result| {
                    if c.browse[browse].generation != generation {
                        return;
                    }
                    match result {
                        Ok(metas) => {
                            let items = metas.iter().take(40).map(RowItem::from_meta).collect();
                            c.fill_row(browse, row, items);
                        }
                        Err(e) => {
                            tracing::warn!("row {row}: {e}");
                            c.browse[browse].rows[row].empty_text = e.to_string();
                            c.fill_row(browse, row, Vec::new());
                        }
                    }
                });
            }
        }
    }

    fn fill_row(&mut self, browse: usize, row: usize, items: Vec<RowItem>) {
        {
            let r = &mut self.browse[browse].rows[row];
            r.set_items(items);
            let data = r.to_row();
            let b = &self.browse[browse];
            if row < b.model.row_count() {
                b.model.set_row_data(row, data);
            }
        }
        // If focus sat on an empty row, move to the first one with content.
        let is_current = self.section == self.browse[browse].section && self.screen == crate::Screen::Browse;
        let b = &mut self.browse[browse];
        let current_dead = !b.rows[b.row].is_navigable() && !b.rows[b.row].loading;
        if (current_dead || !b.user_moved)
            && let Some(first) = b.rows.iter().position(|r| r.is_navigable() || (r.loading && !b.user_moved))
            && b.rows[first].is_navigable()
        {
            b.row = first;
        }
        if is_current {
            self.push_browse_focus(browse);
            self.load_visible_rows(browse);
        }
    }

    fn load_visible_rows(&mut self, browse: usize) {
        let current = self.browse[browse].row;
        for row in current.saturating_sub(1)..(current + 4).min(self.browse[browse].rows.len()) {
            let (model, urls, kind, focused) = {
                let r = &self.browse[browse].rows[row];
                (r.model.clone(), r.image_urls(), r.image_kind(), r.focused)
            };
            let end = (focused + 9).min(urls.len());
            self.load_posters(&model, &urls, focused.saturating_sub(1)..end, kind);
        }
    }

    /// Push focus, scroll and hero for the visible browse section.
    fn push_browse_focus(&mut self, browse: usize) {
        let b = &self.browse[browse];
        let state = BrowseState::get(&self.ui);
        state.set_focused_row(b.row as i32);
        let scroll: f32 = b.rows[..b.row.min(b.rows.len())].iter().map(RowCtl::height).sum();
        state.set_scroll_y(scroll);
        self.update_hero(browse);
    }

    fn update_hero(&mut self, browse: usize) {
        let Some(item) = self.browse[browse].focused_item().cloned() else { return };
        let hero = &item.hero;
        let state = BrowseState::get(&self.ui);
        let logo = hero.logo.as_deref().and_then(|url| self.images.request(url, ImageKind::Logo, ImageSlot::Named));
        state.set_hero(Hero {
            title: shared(&hero.title),
            has_logo: logo.is_some(),
            logo: logo.unwrap_or_default(),
            meta: shared(&hero.meta),
            rating: shared(&hero.rating),
            description: shared(&hero.description),
        });
        self.browse[browse].hero_logo = hero.logo.clone();
        self.browse[browse].hero_backdrop = hero.backdrop.clone();
        if let Some(url) = hero.backdrop.clone()
            && let Some(image) = self.images.request(&url, ImageKind::Backdrop, ImageSlot::Named)
        {
            self.show_backdrop(browse, &url, &image);
        }
    }

    fn show_backdrop(&mut self, browse: usize, url: &str, image: &Image) {
        let b = &mut self.browse[browse];
        if b.shown_backdrop.as_deref() == Some(url) {
            return;
        }
        b.shown_backdrop = Some(url.to_owned());
        b.show_b = !b.show_b;
        let state = BrowseState::get(&self.ui);
        if b.show_b {
            state.set_backdrop_b(image.clone());
        } else {
            state.set_backdrop_a(image.clone());
        }
        state.set_show_b(b.show_b);
    }

    pub(super) fn browse_backdrop_ready(&mut self, url: &str, image: &Image) {
        if self.screen != crate::Screen::Browse {
            return;
        }
        let browse = self.browse_index(self.section);
        if self.browse[browse].hero_backdrop.as_deref() == Some(url) {
            self.show_backdrop(browse, url, image);
        }
    }

    pub(super) fn browse_logo_ready(&mut self, url: &str, image: &Image) {
        if self.screen != crate::Screen::Browse {
            return;
        }
        let browse = self.browse_index(self.section);
        if self.browse[browse].hero_logo.as_deref() == Some(url) {
            let state = BrowseState::get(&self.ui);
            let mut hero = state.get_hero();
            hero.logo = image.clone();
            hero.has_logo = true;
            state.set_hero(hero);
        }
    }

    pub fn browse_key(&mut self, key: Key) -> bool {
        let browse = self.browse_index(self.section);
        match key {
            Key::Up | Key::Down => {
                let b = &self.browse[browse];
                let mut target = b.row as i32;
                loop {
                    target += if key == Key::Up { -1 } else { 1 };
                    if target < 0 || target as usize >= b.rows.len() {
                        return true;
                    }
                    let row = &b.rows[target as usize];
                    if row.is_navigable() || row.loading {
                        break;
                    }
                }
                self.browse[browse].row = target as usize;
                self.browse[browse].user_moved = true;
                self.push_browse_focus(browse);
                self.load_visible_rows(browse);
            }
            Key::Left | Key::Right => {
                let b = &mut self.browse[browse];
                b.user_moved = true;
                let row_index = b.row;
                let Some(row) = b.rows.get_mut(row_index) else { return true };
                if key == Key::Left {
                    if row.focused == 0 {
                        return self.focus_rail();
                    }
                    row.focused -= 1;
                } else if row.focused + 1 < row.items.len() {
                    row.focused += 1;
                } else {
                    return true;
                }
                let data = row.to_row();
                b.model.set_row_data(row_index, data);
                self.update_hero(browse);
                self.load_visible_rows(browse);
            }
            Key::Ok => {
                if let Some(item) = self.browse[browse].focused_item().cloned() {
                    self.open(item.target);
                }
            }
            Key::Back => return self.focus_rail(),
            _ => {}
        }
        true
    }
}
