//! Discover (filterable grid with paging) and My List.

use std::rc::Rc;

use joshflix_core::MediaKind;
use joshflix_core::cinemeta::{CatalogId, CatalogQuery, GENRES, MetaPreview};
use slint::{Global, ModelRc, VecModel};

use super::browse::{RowCtl, RowItem, RowSource};
use super::{Controller, ImageKind, Key, shared};
use crate::format;
use crate::{DiscoverState, MyListState, Poster, TrackItem};

const COLUMNS: usize = 6;

pub struct DiscoverCtl {
    kind: MediaKind,
    sort: CatalogId,
    genre: Option<String>,
    items: Vec<RowItem>,
    model: Rc<VecModel<Poster>>,
    zone: i32,
    filter: usize,
    index: usize,
    picker: Option<(usize, Vec<String>, usize)>,
    loading: bool,
    exhausted: bool,
    generation: u64,
    loaded: bool,
}

impl DiscoverCtl {
    pub fn new() -> Self {
        Self {
            kind: MediaKind::Movie,
            sort: CatalogId::Top,
            genre: None,
            items: Vec::new(),
            model: Rc::new(VecModel::default()),
            zone: 1,
            filter: 0,
            index: 0,
            picker: None,
            loading: false,
            exhausted: false,
            generation: 0,
            loaded: false,
        }
    }

    fn sort_label(&self) -> &'static str {
        match self.sort {
            CatalogId::Top => "Popular",
            CatalogId::Year => "New",
            CatalogId::ImdbRating => "Top rated",
        }
    }

    fn filter_labels(&self) -> Vec<String> {
        vec![
            if self.kind == MediaKind::Movie { "Movies".into() } else { "Shows".into() },
            self.sort_label().into(),
            self.genre.clone().unwrap_or_else(|| {
                if self.sort == CatalogId::Year { format::current_year().to_string() } else { "All genres".into() }
            }),
        ]
    }

    fn options(&self, filter: usize) -> (String, Vec<String>, usize) {
        match filter {
            0 => ("Type".into(), vec!["Movies".into(), "Shows".into()], usize::from(self.kind == MediaKind::Series)),
            1 => (
                "Sort".into(),
                vec!["Popular".into(), "New".into(), "Top rated".into()],
                match self.sort {
                    CatalogId::Top => 0,
                    CatalogId::Year => 1,
                    CatalogId::ImdbRating => 2,
                },
            ),
            _ => {
                if self.sort == CatalogId::Year {
                    let now = format::current_year();
                    let years: Vec<String> = (1950..=now).rev().map(|y| y.to_string()).collect();
                    let current = self.genre.clone().unwrap_or_else(|| now.to_string());
                    let index = years.iter().position(|y| *y == current).unwrap_or(0);
                    ("Year".into(), years, index)
                } else {
                    let mut genres = vec!["All genres".to_owned()];
                    genres.extend(GENRES.iter().map(|g| (*g).to_owned()));
                    let index = self.genre.as_ref().and_then(|g| genres.iter().position(|x| x == g)).unwrap_or(0);
                    ("Genre".into(), genres, index)
                }
            }
        }
    }

    fn query(&self) -> CatalogQuery {
        let mut q = CatalogQuery::new(self.kind, self.sort).skip(self.items.len() as u32);
        q.genre = match (self.sort, &self.genre) {
            (CatalogId::Year, None) => Some(format::current_year().to_string()),
            (_, genre) => genre.clone(),
        };
        q
    }
}

pub struct MyListCtl {
    cw: RowCtl,
    items: Vec<RowItem>,
    model: Rc<VecModel<Poster>>,
    zone: i32,
    index: usize,
}

impl MyListCtl {
    pub fn new() -> Self {
        Self {
            cw: RowCtl::new("Continue Watching", RowSource::ContinueWatching),
            items: Vec::new(),
            model: Rc::new(VecModel::default()),
            zone: 0,
            index: 0,
        }
    }
}

impl Controller {
    // ---- Discover ----

    pub fn show_discover(&mut self) {
        DiscoverState::get(&self.ui).set_items(ModelRc::from(self.discover.model.clone()));
        if !self.discover.loaded {
            self.discover.loaded = true;
            self.reload_discover();
        }
        self.push_discover();
    }

    fn reload_discover(&mut self) {
        let d = &mut self.discover;
        d.items.clear();
        d.model.set_vec(Vec::new());
        d.index = 0;
        d.exhausted = false;
        d.generation += 1;
        self.load_discover_page();
    }

    fn load_discover_page(&mut self) {
        let d = &mut self.discover;
        if d.loading || d.exhausted {
            return;
        }
        d.loading = true;
        let generation = d.generation;
        let query = d.query();
        let cinemeta = self.svc.cinemeta.clone();
        self.spawn(async move { cinemeta.catalog(&query).await }, move |c, result| {
            if c.discover.generation != generation {
                return;
            }
            c.discover.loading = false;
            match result {
                Ok(metas) => {
                    let known: std::collections::HashSet<String> = c.discover.items.iter().map(|i| i.id.clone()).collect();
                    let fresh: Vec<RowItem> = metas.iter().filter(|m| !known.contains(&m.id)).map(RowItem::from_meta).collect();
                    if fresh.is_empty() {
                        c.discover.exhausted = true;
                    }
                    let start = c.discover.items.len();
                    for item in &fresh {
                        c.discover.model.push(item.to_poster());
                    }
                    c.discover.items.extend(fresh);
                    let urls: Vec<Option<String>> = c.discover.items.iter().map(|i| i.image.clone()).collect();
                    let model = c.discover.model.clone();
                    let end = (start + COLUMNS * 3).min(urls.len());
                    c.load_posters(&model, &urls, start..end, ImageKind::Poster);
                }
                Err(e) => c.toast(&e.to_string()),
            }
            c.push_discover();
        });
        self.push_discover();
    }

    fn push_discover(&mut self) {
        let d = &self.discover;
        let state = DiscoverState::get(&self.ui);
        let labels: Vec<slint::SharedString> = d.filter_labels().iter().map(shared).collect();
        state.set_filters(ModelRc::new(VecModel::from(labels)));
        state.set_loading(d.loading);
        state.set_zone(d.zone);
        state.set_filter_index(d.filter as i32);
        state.set_grid_index(d.index as i32);
        match &d.picker {
            Some((filter, options, index)) => {
                let (title, _, current) = d.options(*filter);
                let items: Vec<TrackItem> = options
                    .iter()
                    .enumerate()
                    .map(|(i, o)| TrackItem { label: shared(o), selected: i == current })
                    .collect();
                state.set_picker_open(true);
                state.set_picker_title(shared(title));
                state.set_picker_options(ModelRc::new(VecModel::from(items)));
                state.set_picker_index(*index as i32);
            }
            None => state.set_picker_open(false),
        }
    }

    fn discover_focus_changed(&mut self) {
        let index = self.discover.index;
        let urls: Vec<Option<String>> = self.discover.items.iter().map(|i| i.image.clone()).collect();
        let model = self.discover.model.clone();
        let line = index / COLUMNS;
        let start = line.saturating_sub(1) * COLUMNS;
        let end = ((line + 4) * COLUMNS).min(urls.len());
        self.load_posters(&model, &urls, start..end, ImageKind::Poster);
        if index + COLUMNS * 2 >= self.discover.items.len() {
            self.load_discover_page();
        }
    }

    pub fn discover_key(&mut self, key: Key) -> bool {
        if let Some((filter, options, index)) = self.discover.picker.as_mut() {
            match key {
                Key::Up => *index = index.saturating_sub(1),
                Key::Down => *index = (*index + 1).min(options.len() - 1),
                Key::Back => self.discover.picker = None,
                Key::Ok => {
                    let (filter, choice) = (*filter, options[*index].clone());
                    self.discover.picker = None;
                    let d = &mut self.discover;
                    match filter {
                        0 => d.kind = if choice == "Shows" { MediaKind::Series } else { MediaKind::Movie },
                        1 => {
                            d.sort = match choice.as_str() {
                                "New" => CatalogId::Year,
                                "Top rated" => CatalogId::ImdbRating,
                                _ => CatalogId::Top,
                            };
                            d.genre = None;
                        }
                        _ => d.genre = (choice != "All genres").then_some(choice),
                    }
                    self.reload_discover();
                }
                _ => {}
            }
            self.push_discover();
            return true;
        }
        let d = &mut self.discover;
        match (d.zone, key) {
            (0, Key::Left) => {
                if d.filter == 0 {
                    return self.focus_rail();
                }
                d.filter -= 1;
            }
            (0, Key::Right) => d.filter = (d.filter + 1).min(2),
            (0, Key::Down) if !d.items.is_empty() => d.zone = 1,
            (0, Key::Ok) => {
                let (_, options, current) = d.options(d.filter);
                d.picker = Some((d.filter, options, current));
            }
            (0, Key::Back) => return self.focus_rail(),
            (1, Key::Left) => {
                if d.index % COLUMNS == 0 {
                    return self.focus_rail();
                }
                d.index -= 1;
            }
            (1, Key::Right) => {
                if d.index % COLUMNS + 1 < COLUMNS && d.index + 1 < d.items.len() {
                    d.index += 1;
                }
            }
            (1, Key::Up) => {
                if d.index < COLUMNS {
                    d.zone = 0;
                } else {
                    d.index -= COLUMNS;
                }
            }
            (1, Key::Down) => {
                if d.index + COLUMNS < d.items.len() {
                    d.index += COLUMNS;
                } else if d.index / COLUMNS < (d.items.len().saturating_sub(1)) / COLUMNS {
                    d.index = d.items.len() - 1;
                }
            }
            (1, Key::Ok) => {
                if let Some(item) = d.items.get(d.index).cloned() {
                    self.open(item.target);
                }
                return true;
            }
            (1, Key::Back) => d.zone = 0,
            _ => {}
        }
        self.discover_focus_changed();
        self.push_discover();
        true
    }

    // ---- My List ----

    pub fn show_mylist(&mut self) {
        let cw: Vec<RowItem> = self
            .svc
            .store
            .continue_watching(20)
            .unwrap_or_default()
            .iter()
            .map(RowItem::from_progress)
            .collect();
        self.mylist.cw.set_items(cw);
        let items: Vec<RowItem> = self
            .svc
            .store
            .library()
            .unwrap_or_default()
            .iter()
            .map(|i| {
                RowItem::from_meta(&MetaPreview {
                    id: i.id.clone(),
                    kind: i.kind,
                    name: i.name.clone(),
                    poster: i.poster.clone(),
                    background: i.background.clone(),
                    logo: i.logo.clone(),
                    description: None,
                    release_info: i.release_info.clone(),
                    imdb_rating: None,
                    genres: vec![],
                    runtime: None,
                })
            })
            .collect();
        self.mylist.model.set_vec(items.iter().map(RowItem::to_poster).collect::<Vec<_>>());
        self.mylist.items = items;
        let m = &mut self.mylist;
        if m.cw.items.is_empty() {
            m.zone = 1;
        } else if m.items.is_empty() {
            m.zone = 0;
        }
        m.index = m.index.min(m.items.len().saturating_sub(1));
        let state = MyListState::get(&self.ui);
        state.set_items(ModelRc::from(self.mylist.model.clone()));
        let (cw_model, cw_urls) = (self.mylist.cw.model.clone(), self.mylist.cw.image_urls());
        self.load_posters(&cw_model, &cw_urls, 0..cw_urls.len().min(8), ImageKind::Wide);
        let (model, urls) = (self.mylist.model.clone(), self.mylist.items.iter().map(|i| i.image.clone()).collect::<Vec<_>>());
        self.load_posters(&model, &urls, 0..urls.len().min(COLUMNS * 4), ImageKind::Poster);
        self.push_mylist();
    }

    fn push_mylist(&self) {
        let m = &self.mylist;
        let state = MyListState::get(&self.ui);
        state.set_continue_row(m.cw.to_row());
        state.set_zone(m.zone);
        state.set_grid_index(m.index as i32);
    }

    pub fn mylist_key(&mut self, key: Key) -> bool {
        let m = &mut self.mylist;
        match (m.zone, key) {
            (_, Key::Back) => return self.focus_rail(),
            (0, Key::Left) => {
                if m.cw.focused == 0 {
                    return self.focus_rail();
                }
                m.cw.focused -= 1;
            }
            (0, Key::Right) => m.cw.focused = (m.cw.focused + 1).min(m.cw.items.len().saturating_sub(1)),
            (0, Key::Down) if !m.items.is_empty() => m.zone = 1,
            (0, Key::Ok) => {
                if let Some(item) = m.cw.items.get(m.cw.focused).cloned() {
                    self.open(item.target);
                }
                return true;
            }
            (1, Key::Left) => {
                if m.index % COLUMNS == 0 {
                    return self.focus_rail();
                }
                m.index -= 1;
            }
            (1, Key::Right) => {
                if m.index % COLUMNS + 1 < COLUMNS && m.index + 1 < m.items.len() {
                    m.index += 1;
                }
            }
            (1, Key::Up) => {
                if m.index < COLUMNS {
                    if !m.cw.items.is_empty() {
                        m.zone = 0;
                    }
                } else {
                    m.index -= COLUMNS;
                }
            }
            (1, Key::Down) => {
                if m.index + COLUMNS < m.items.len() {
                    m.index += COLUMNS;
                }
            }
            (1, Key::Ok) => {
                if let Some(item) = m.items.get(m.index).cloned() {
                    self.open(item.target);
                }
                return true;
            }
            _ => {}
        }
        self.push_mylist();
        true
    }
}
