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
        }
    }
}

impl Controller {
    pub fn show_search(&mut self) {
        let state = SearchState::get(&self.ui);
        state.set_keys(self.search.keyboard.model(&crate::Icons::get(&self.ui)));
        state.set_rows(ModelRc::from(self.search.model.clone()));
        self.push_search();
    }

    fn push_search(&self) {
        let s = &self.search;
        let state = SearchState::get(&self.ui);
        state.set_query(shared(&s.query));
        state.set_key_index(s.keyboard.index as i32);
        state.set_keyboard_active(s.keyboard_active);
        state.set_focused_row(s.row as i32);
        state.set_searching(s.searching > 0);
        let has_results = s.rows.iter().any(RowCtl::is_navigable);
        let hint = if s.query.trim().len() < 2 {
            "Type at least two letters to search movies and shows."
        } else if s.searching == 0 && !has_results {
            "No matches. Try a different spelling."
        } else {
            ""
        };
        state.set_hint(hint.into());
    }

    fn query_changed(&mut self) {
        self.push_search();
        // Search 350 ms after the last key press.
        self.search.debounce.start(slint::TimerMode::SingleShot, Duration::from_millis(350), || {
            with_controller(Controller::run_search);
        });
    }

    fn run_search(&mut self) {
        let query = self.search.query.trim().to_owned();
        self.search.generation += 1;
        let generation = self.search.generation;
        if query.len() < 2 {
            self.search.rows.clear();
            self.search.row = 0;
            self.search.keyboard_active = true; // nothing to focus below the keyboard
            self.search.model.set_vec(Vec::<Row>::new());
            self.search.searching = 0;
            self.push_search();
            return;
        }
        self.search.rows = vec![
            RowCtl::new("Movies", RowSource::Catalog(joshflix_core::cinemeta::CatalogQuery::search(MediaKind::Movie, &query))),
            RowCtl::new("Shows", RowSource::Catalog(joshflix_core::cinemeta::CatalogQuery::search(MediaKind::Series, &query))),
        ];
        self.search.row = 0;
        self.search.model.set_vec(self.search.rows.iter().map(RowCtl::to_row).collect::<Vec<_>>());
        self.search.searching = 2;
        self.push_search();
        for (index, kind) in [(0usize, MediaKind::Movie), (1, MediaKind::Series)] {
            let cinemeta = self.svc.cinemeta.clone();
            let q = query.clone();
            self.spawn(async move { cinemeta.search(kind, &q).await }, move |c, result| {
                if c.search.generation != generation {
                    return;
                }
                c.search.searching = c.search.searching.saturating_sub(1);
                let items: Vec<RowItem> = result.unwrap_or_default().iter().take(30).map(RowItem::from_meta).collect();
                let Some(row) = c.search.rows.get_mut(index) else { return };
                row.empty_text = format!("No {} found", if index == 0 { "movies" } else { "shows" });
                row.set_items(items);
                let data = row.to_row();
                if index < c.search.model.row_count() {
                    c.search.model.set_row_data(index, data);
                }
                let (model, urls) = (row.model.clone(), row.image_urls());
                let end = urls.len().min(8);
                c.load_posters(&model, &urls, 0..end, super::ImageKind::Poster);
                if (c.search.keyboard_active || !c.search.rows[c.search.row].is_navigable())
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
                    self.search.keyboard.step(0, -1);
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
                Key::Back => {
                    if !self.search.query.is_empty() {
                        self.search.query.pop();
                        self.query_changed();
                    } else {
                        return self.focus_rail();
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
