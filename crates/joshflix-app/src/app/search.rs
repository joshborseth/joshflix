//! Search: our own on-screen keyboard with live results.

use std::time::Duration;

use joshflix_core::MediaKind;
use slint::{Global, Model, ModelRc, VecModel};

use super::browse::{RowCtl, RowItem, RowSource};
use super::{Controller, Key, shared, with_controller};
use crate::keyboard::{self, KeyAction, Keyboard};
use crate::{Row, SearchState};

pub struct SearchCtl {
    pub query: String,
    keyboard: Keyboard,
    keyboard_active: bool,
    rows: Vec<RowCtl>,
    row: usize,
    model: std::rc::Rc<VecModel<Row>>,
    generation: u64,
    searching: usize,
    debounce: slint::Timer,
    /// Blinks the caret twice a second (only while Search shows).
    caret: slint::Timer,
    caret_on: bool,
    /// Focus is on the recent-search chips (which one).
    recent_focus: Option<usize>,
    /// The chips were entered from the keyboard (Down goes back there).
    recent_from_keyboard: bool,
}

/// Caret blink half-period.
const CARET_BLINK: Duration = Duration::from_millis(530);

/// Recent searches kept and shown.
const RECENT_SHOWN: usize = 4;

impl SearchCtl {
    pub(super) fn focused_item(&self) -> Option<&RowItem> {
        if self.keyboard_active {
            return None;
        }
        let row = self.rows.get(self.row)?;
        row.items.get(row.focused)
    }
}

impl SearchCtl {
    pub fn new() -> Self {
        Self {
            query: String::new(),
            keyboard: Keyboard::search(),
            keyboard_active: true,
            rows: Vec::new(),
            row: 0,
            model: std::rc::Rc::new(VecModel::default()),
            generation: 0,
            searching: 0,
            debounce: slint::Timer::default(),
            caret: slint::Timer::default(),
            caret_on: true,
            recent_focus: None,
            recent_from_keyboard: false,
        }
    }
}

impl Controller {
    pub fn show_search(&mut self) {
        let state = SearchState::get(&self.ui);
        state.set_keys(self.search.keyboard.model(&crate::Icons::get(&self.ui)));
        state.set_rows(ModelRc::from(self.search.model.clone()));
        // Popular titles until something is typed (again, if they failed last time).
        let failed = self.search.searching == 0 && !self.search.rows.iter().any(RowCtl::is_navigable);
        if self.search.rows.is_empty() || (self.search.query.trim().len() < 2 && failed) {
            self.run_search();
        }
        self.push_search();
    }

    fn push_search(&mut self) {
        // Solid while keys are pressed, blinking once they stop.
        self.search.caret_on = true;
        self.search.caret.start(slint::TimerMode::Repeated, CARET_BLINK, || {
            with_controller(|c| {
                if c.screen != crate::Screen::Search {
                    c.search.caret.stop();
                    return;
                }
                c.search.caret_on = !c.search.caret_on;
                SearchState::get(&c.ui).set_caret_on(c.search.caret_on);
            });
        });
        let s = &self.search;
        let state = SearchState::get(&self.ui);
        state.set_caret_on(true);
        state.set_query(shared(&s.query));
        state.set_key_index(s.keyboard.index as i32);
        state.set_keyboard_active(s.keyboard_active);
        state.set_focused_row(s.row as i32);
        // The chips' model changes only when the list does (not on every key).
        let recent: Vec<slint::SharedString> = self.settings.recent_searches.iter().take(RECENT_SHOWN).map(shared).collect();
        if state.get_recent().iter().ne(recent.iter().cloned()) {
            state.set_recent(ModelRc::new(VecModel::from(recent)));
        }
        state.set_recent_focused(s.recent_focus.map_or(-1, |i| i as i32));
        state.set_searching(s.searching > 0);
        let has_results = s.rows.iter().any(RowCtl::is_navigable);
        let hint = if s.query.trim().len() == 1 {
            "Type at least two letters to search movies and shows."
        } else if s.searching == 0 && !has_results && s.query.trim().is_empty() {
            "Couldn't load popular titles. Check the TV's internet connection, or type to search."
        } else if s.searching == 0 && !has_results {
            "No matches. Try a different spelling."
        } else {
            ""
        };
        state.set_hint(hint.into());
    }

    /// The recent-search chips show (the same rule as search.slint's `show-recent`).
    fn recent_shown(&self) -> bool {
        self.search.query.is_empty() && !self.settings.recent_searches.is_empty() && self.search.searching == 0
    }

    fn query_changed(&mut self) {
        self.search.recent_focus = None;
        self.push_search();
        // Search 350 ms after the last key press.
        self.search.debounce.start(slint::TimerMode::SingleShot, Duration::from_millis(350), || {
            with_controller(Controller::run_search);
        });
    }

    fn run_search(&mut self) {
        let query = self.search.query.trim().to_owned();
        use joshflix_core::cinemeta::{CatalogId, CatalogQuery};
        // Under two letters: popular titles, so the screen is never empty.
        let popular = query.len() < 2;
        // Already showing them (typing a first letter, or deleting back to one): keep them.
        let showing_popular = self.search.rows.first().is_some_and(|r| r.title == "Popular Movies")
            && self.search.rows.iter().any(|r| r.loading || r.is_navigable());
        if popular && showing_popular {
            self.push_search();
            return;
        }
        self.search.generation += 1;
        let generation = self.search.generation;
        let queries = if popular {
            [
                ("Popular Movies", CatalogQuery::new(MediaKind::Movie, CatalogId::Top)),
                ("Popular Shows", CatalogQuery::new(MediaKind::Series, CatalogId::Top)),
            ]
        } else {
            [("Movies", CatalogQuery::search(MediaKind::Movie, &query)), ("Shows", CatalogQuery::search(MediaKind::Series, &query))]
        };
        if popular {
            self.search.keyboard_active = true;
        }
        self.search.rows = queries.iter().map(|(title, q)| RowCtl::new(*title, RowSource::Catalog(q.clone()))).collect();
        self.search.row = 0;
        self.search.model.set_vec(self.search.rows.iter().map(RowCtl::to_row).collect::<Vec<_>>());
        self.search.searching = 2;
        self.push_search();
        for (index, (_, q)) in queries.into_iter().enumerate() {
            let cinemeta = self.svc.cinemeta.clone();
            let fetch = async move {
                // Home keeps these catalogs on disk: no wait for the network.
                if popular && let Some(saved) = cinemeta.cached_catalog(&q).await {
                    return Ok(saved);
                }
                cinemeta.catalog(&q).await
            };
            self.spawn(fetch, move |c, result| {
                if c.search.generation != generation {
                    return;
                }
                c.search.searching = c.search.searching.saturating_sub(1);
                let noun = if index == 0 { "movies" } else { "shows" };
                let (items, empty_text) = match result {
                    Ok(found) => (found.iter().take(30).map(RowItem::from_meta).collect::<Vec<_>>(), format!("No {noun} found")),
                    Err(e) if popular => (Vec::new(), format!("Couldn't load popular {noun}. {e}")),
                    Err(e) => (Vec::new(), format!("Couldn't search {noun}. {e}")),
                };
                let Some(row) = c.search.rows.get_mut(index) else { return };
                row.empty_text = empty_text;
                row.set_items(items);
                let data = row.to_row();
                if index < c.search.model.row_count() {
                    c.search.model.set_row_data(index, data);
                }
                let (model, urls) = (row.model.clone(), row.image_urls());
                let end = urls.len().min(8);
                c.load_posters(&model, &urls, 0..end, super::ImageKind::Poster);
                if (c.search.keyboard_active || c.search.recent_focus.is_none() && !c.search.rows.get(c.search.row).is_some_and(RowCtl::is_navigable))
                    && let Some(first) = c.search.rows.iter().position(RowCtl::is_navigable)
                {
                    c.search.row = first;
                }
                c.push_search();
            });
        }
    }

    pub fn search_key(&mut self, key: Key) -> bool {
        // Physical keyboards (desktop) type directly.
        match key {
            Key::Char(c) if c.is_alphanumeric() || c == ' ' => {
                let action = if c == ' ' { KeyAction::Space } else { KeyAction::Char(c.to_ascii_lowercase()) };
                if keyboard::apply(&mut self.search.query, action, 60) {
                    self.query_changed();
                }
                return true;
            }
            Key::Backspace => {
                if keyboard::apply(&mut self.search.query, KeyAction::Backspace, 60) {
                    self.query_changed();
                }
                return true;
            }
            _ => {}
        }

        if self.search.keyboard_active {
            match key {
                Key::Left => {
                    if !self.search.keyboard.step(-1, 0) {
                        return self.focus_rail();
                    }
                }
                Key::Right => {
                    if !self.search.keyboard.step(1, 0) && self.search.rows.iter().any(RowCtl::is_navigable) {
                        self.search.keyboard_active = false;
                    }
                }
                Key::Up => {
                    // From the top row: the recent searches, when they show.
                    if !self.search.keyboard.step(0, -1) && self.recent_shown() {
                        self.search.keyboard_active = false;
                        self.search.recent_focus = Some(0);
                        self.search.recent_from_keyboard = true;
                        // Results unscrolled, so the chips above them show.
                        let first = self.search.rows.iter().position(RowCtl::is_navigable).unwrap_or(0);
                        self.search.row = first;
                        if let Some(row) = self.search.rows.get_mut(first)
                            && row.focused != 0
                        {
                            row.focused = 0;
                            let data = row.to_row();
                            self.search.model.set_row_data(first, data);
                        }
                    }
                }
                Key::Down => {
                    self.search.keyboard.step(0, 1);
                }
                Key::Ok => {
                    let action = self.search.keyboard.current().action;
                    if keyboard::apply(&mut self.search.query, action, 60) {
                        self.query_changed();
                    }
                }
                // Back leaves (the query stays); the ⌫ key deletes.
                Key::Back => return self.focus_rail(),
                _ => {}
            }
            self.push_search();
            return true;
        }

        // Recent-search chips (above the results while nothing is typed).
        if let Some(index) = self.search.recent_focus {
            let count = self.settings.recent_searches.len();
            match key {
                Key::Left if index > 0 => self.search.recent_focus = Some(index - 1),
                Key::Left | Key::Back => {
                    self.search.recent_focus = None;
                    self.search.keyboard_active = true;
                }
                Key::Right => self.search.recent_focus = Some((index + 1).min(count.min(RECENT_SHOWN).saturating_sub(1))),
                Key::Down => {
                    self.search.recent_focus = None;
                    // Back to wherever focus came from: the keyboard, or the results.
                    if self.search.recent_from_keyboard || !self.search.rows.iter().any(RowCtl::is_navigable) {
                        self.search.keyboard_active = true;
                    }
                }
                Key::Ok => {
                    self.search.recent_focus = None;
                    // Focus goes to the first row with results once they come.
                    self.search.row = 0;
                    if let Some(query) = self.settings.recent_searches.get(index).cloned() {
                        self.search.query = query;
                        self.search.debounce.stop();
                        self.run_search();
                    }
                }
                _ => {}
            }
            self.push_search();
            return true;
        }

        // Results.
        let row_index = self.search.row;
        if self.search.rows.get(row_index).is_none() {
            self.search.keyboard_active = true;
            self.push_search();
            return true;
        }
        match key {
            Key::Up | Key::Down => {
                let target = if key == Key::Up { row_index.checked_sub(1) } else { Some(row_index + 1) };
                if let Some(t) = target.filter(|t| self.search.rows.get(*t).is_some_and(RowCtl::is_navigable)) {
                    self.search.row = t;
                } else if key == Key::Up && self.recent_shown() {
                    self.search.recent_focus = Some(0);
                    self.search.recent_from_keyboard = false;
                    // The row behind the chips goes back to its start.
                    let row = &mut self.search.rows[row_index];
                    if row.focused != 0 {
                        row.focused = 0;
                        let data = row.to_row();
                        self.search.model.set_row_data(row_index, data);
                    }
                }
            }
            Key::Left => {
                let row = &mut self.search.rows[row_index];
                if row.focused == 0 {
                    self.search.keyboard_active = true;
                } else {
                    row.focused -= 1;
                    let data = row.to_row();
                    self.search.model.set_row_data(row_index, data);
                }
            }
            Key::Right => {
                let row = &mut self.search.rows[row_index];
                if row.focused + 1 < row.items.len() {
                    row.focused += 1;
                    let data = row.to_row();
                    self.search.model.set_row_data(row_index, data);
                    let (model, urls, f) = (row.model.clone(), row.image_urls(), row.focused);
                    let end = (f + 8).min(urls.len());
                    self.load_posters(&model, &urls, f..end, super::ImageKind::Poster);
                }
            }
            Key::Ok => {
                let row = &self.search.rows[row_index];
                if let Some(item) = row.items.get(row.focused).cloned() {
                    // A search that found something is worth offering again.
                    let query = self.search.query.trim().to_owned();
                    if query.len() >= 2 {
                        self.save_settings(|s| {
                            s.recent_searches.retain(|q| !q.eq_ignore_ascii_case(&query));
                            s.recent_searches.insert(0, query);
                            s.recent_searches.truncate(RECENT_SHOWN);
                        });
                    }
                    self.open(item.target);
                }
                return true;
            }
            Key::Back => self.search.keyboard_active = true,
            _ => {}
        }
        self.push_search();
        true
    }
}
