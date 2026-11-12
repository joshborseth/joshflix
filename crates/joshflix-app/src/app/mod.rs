//! The controller: owns all app state and focus, reacts to keys and async
//! results, and pushes view state into the Slint globals.
//!
//! It lives in a thread-local on the UI thread. Background work runs on a
//! tokio runtime and reports back with [`Controller::spawn`], which hops to
//! the UI thread through `slint::invoke_from_event_loop`.

mod browse;
mod detail;
mod dialog;
mod grids;
mod images;
mod pairing;
mod player;
mod search;
mod settings;
mod sources;

use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use joshflix_core::cinemeta::{Cinemeta, MetaPreview};
use joshflix_core::http::Http;
use joshflix_core::images::ImageLoader;
use joshflix_core::pairing::Relay;
use joshflix_core::release::DisplayCaps;
use joshflix_core::store::{Settings, Store};
use joshflix_core::subtitles::OpenSubtitles;
use joshflix_core::update::Updater;
use joshflix_core::{MediaKind, VideoRef};
use slint::{ComponentHandle, Global, Model, ModelRc, SharedString, VecModel};

use crate::platform::Platform;
use crate::video::VideoHost;
use crate::{
    AppState, AppWindow, BrowseState, Icons, NavItem, Poster, RailState, Screen,
};

pub use browse::BrowseCtl;
pub use images::{ImageCache, ImageKind, ImageSlot};

/// Shared services, cheap to clone into background tasks.
#[derive(Clone)]
pub struct Services {
    pub http: Http,
    pub cinemeta: Cinemeta,
    pub subtitles: OpenSubtitles,
    pub store: Store,
    pub images: ImageLoader,
    pub relay: Relay,
    pub updater: Updater,
}

/// Remote keys after mapping in `app.slint`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Left,
    Right,
    Up,
    Down,
    Ok,
    Back,
    Backspace,
    Menu,
    PageUp,
    PageDown,
    Char(char),
}

impl Key {
    /// Parse the strings `app.slint` sends. Returns (key, is_repeat).
    pub fn parse(raw: &str) -> Option<(Self, bool)> {
        let (base, repeat) = match raw.strip_suffix('+') {
            Some(base) if base == "left" || base == "right" => (base, true),
            _ => (raw, false),
        };
        let key = match base {
            "left" => Self::Left,
            "right" => Self::Right,
            "up" => Self::Up,
            "down" => Self::Down,
            "ok" => Self::Ok,
            "back" => Self::Back,
            "backspace" => Self::Backspace,
            "menu" => Self::Menu,
            "pageup" => Self::PageUp,
            "pagedown" => Self::PageDown,
            other => {
                let c = other.strip_prefix("char:")?.chars().next()?;
                if c.is_control() || ('\u{f700}'..='\u{f8ff}').contains(&c) {
                    return None;
                }
                Self::Char(c)
            }
        };
        Some((key, repeat))
    }
}

/// Top-level sections reachable from the rail, in rail order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Search,
    Home,
    Movies,
    Shows,
    Discover,
    MyList,
    Settings,
}

pub const SECTIONS: [Section; 7] = [
    Section::Search,
    Section::Home,
    Section::Movies,
    Section::Shows,
    Section::Discover,
    Section::MyList,
    Section::Settings,
];

impl Section {
    fn index(self) -> usize {
        SECTIONS.iter().position(|s| *s == self).unwrap_or(1)
    }
}

/// What a card opens.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenTarget {
    pub kind: MediaKind,
    pub id: String,
    pub preview: Option<MetaPreview>,
    /// Land on the Resume button (Continue Watching).
    pub focus_resume: bool,
}

thread_local! {
    static CONTROLLER: RefCell<Option<Controller>> = const { RefCell::new(None) };
}

/// Run `f` with the controller (no-op before init or during re-entrant use).
pub fn with_controller(f: impl FnOnce(&mut Controller)) {
    CONTROLLER.with(|cell| {
        if let Ok(mut slot) = cell.try_borrow_mut()
            && let Some(controller) = slot.as_mut()
        {
            f(controller);
        }
    });
}

pub fn install(controller: Controller) {
    CONTROLLER.with(|cell| *cell.borrow_mut() = Some(controller));
}

pub fn uninstall() {
    CONTROLLER.with(|cell| {
        if let Ok(mut slot) = cell.try_borrow_mut()
            && let Some(mut controller) = slot.take()
        {
            controller.shutdown();
        }
    });
}

pub struct Controller {
    pub ui: AppWindow,
    pub svc: Services,
    pub rt: tokio::runtime::Handle,
    pub platform: Rc<dyn Platform>,
    pub video: Rc<VideoHost>,
    pub settings: Settings,
    pub caps: DisplayCaps,
    pub images: ImageCache,

    pub screen: Screen,
    pub section: Section,
    pub rail_focus: bool,
    pub rail_index: usize,
    /// Screens to return to with Back (detail → player stack).
    pub back_stack: Vec<Screen>,

    pub browse: Vec<BrowseCtl>,
    pub search: search::SearchCtl,
    pub discover: grids::DiscoverCtl,
    pub mylist: grids::MyListCtl,
    pub settings_ctl: settings::SettingsCtl,
    pub detail: Option<detail::DetailCtl>,
    pub sources: sources::SourcesCtl,
    pub player: Option<player::PlayerCtl>,
    pub pairing: pairing::PairingCtl,
    pub onboarding_step: i32,
    pub dialog: Option<dialog::DialogCtl>,

    toast_timer: slint::Timer,
    tick_timer: slint::Timer,
    pub update_available: Option<joshflix_core::update::AvailableUpdate>,
}

impl Controller {
    pub fn new(
        ui: AppWindow,
        svc: Services,
        rt: tokio::runtime::Handle,
        platform: Rc<dyn Platform>,
        video: Rc<VideoHost>,
    ) -> Self {
        let settings = svc.store.settings().unwrap_or_default();
        let caps = platform.display_caps();
        let images = ImageCache::new(svc.images.clone(), rt.clone(), ui.window().scale_factor());
        Self {
            ui,
            rt,
            platform,
            video,
            caps,
            images,
            screen: Screen::Onboarding,
            section: Section::Home,
            rail_focus: false,
            rail_index: Section::Home.index(),
            back_stack: Vec::new(),
            browse: vec![
                BrowseCtl::new(Section::Home),
                BrowseCtl::new(Section::Movies),
                BrowseCtl::new(Section::Shows),
            ],
            search: search::SearchCtl::new(),
            discover: grids::DiscoverCtl::new(),
            mylist: grids::MyListCtl::new(),
            settings_ctl: settings::SettingsCtl::new(),
            detail: None,
            sources: sources::SourcesCtl::default(),
            player: None,
            pairing: pairing::PairingCtl::new(),
            onboarding_step: 0,
            dialog: None,
            toast_timer: slint::Timer::default(),
            tick_timer: slint::Timer::default(),
            update_available: None,
            settings,
            svc,
        }
    }

    /// First paint: onboarding or Home.
    pub fn start(&mut self) {
        let icons = Icons::get(&self.ui);
        let nav: Vec<NavItem> = [
            ("Search", icons.get_search()),
            ("Home", icons.get_home()),
            ("Movies", icons.get_film()),
            ("Shows", icons.get_tv()),
            ("Discover", icons.get_compass()),
            ("My List", icons.get_bookmark()),
            ("Settings", icons.get_settings()),
        ]
        .into_iter()
        .map(|(label, icon)| NavItem { label: label.into(), icon })
        .collect();
        RailState::get(&self.ui).set_items(ModelRc::new(VecModel::from(nav)));

        self.images.set_scale(self.ui.window().scale_factor());
        // A 250 ms heartbeat drives the player overlay, countdowns and timers.
        self.tick_timer.start(slint::TimerMode::Repeated, Duration::from_millis(250), || {
            with_controller(Controller::tick);
        });

        // Desktop development: JOSHFLIX_RD_KEY skips onboarding with a given key.
        #[cfg(not(target_os = "android"))]
        if let Ok(key) = std::env::var("JOSHFLIX_RD_KEY")
            && !self.settings.has_rd_key()
        {
            self.save_settings(|s| {
                s.rd_api_key = Some(key);
                s.onboarding_done = true;
            });
        }
        if self.settings.onboarding_done && self.settings.has_rd_key() {
            self.go_section(Section::Home);
            self.refresh_account_silently();
            self.check_for_update(false);
        } else {
            self.show_onboarding();
        }
        self.images.prune_later();
    }

    pub fn shutdown(&mut self) {
        self.stop_playback(true);
        self.pairing.cancel();
        if let Some(player) = self.video.player() {
            player.shutdown();
        }
    }

    // ---- async bridge ----

    /// Run `future` on the runtime, then `done` on the UI thread with the controller.
    pub fn spawn<T: Send + 'static>(
        &self,
        future: impl Future<Output = T> + Send + 'static,
        done: impl FnOnce(&mut Controller, T) + Send + 'static,
    ) {
        self.rt.spawn(async move {
            let value = future.await;
            let _ = slint::invoke_from_event_loop(move || with_controller(|c| done(c, value)));
        });
    }

    pub fn rd_key(&self) -> Option<String> {
        self.settings.rd_api_key.clone().filter(|k| !k.trim().is_empty())
    }

    pub fn save_settings(&mut self, change: impl FnOnce(&mut Settings)) {
        match self.svc.store.update_settings(change) {
            Ok(settings) => self.settings = settings,
            Err(e) => self.toast(&format!("Couldn't save settings: {e}")),
        }
    }

    // ---- toast ----

    pub fn toast(&mut self, message: &str) {
        let state = AppState::get(&self.ui);
        state.set_toast(message.into());
        state.set_toast_shown(true);
        let weak = self.ui.as_weak();
        self.toast_timer.start(slint::TimerMode::SingleShot, Duration::from_millis(3200), move || {
            if let Some(ui) = weak.upgrade() {
                AppState::get(&ui).set_toast_shown(false);
            }
        });
    }

    // ---- screens & navigation ----

    pub fn set_screen(&mut self, screen: Screen) {
        self.screen = screen;
        let state = AppState::get(&self.ui);
        state.set_screen(screen);
        state.set_video_visible(screen == Screen::Player && self.player.as_ref().is_some_and(|p| p.video_shown));
        self.video.set_visible(state.get_video_visible());
        self.ui.window().request_redraw();
    }

    fn set_rail(&mut self, focus: bool) {
        self.rail_focus = focus;
        let rail = RailState::get(&self.ui);
        rail.set_expanded(focus);
        rail.set_focused_index(self.rail_index as i32);
        rail.set_selected(self.section.index() as i32);
        AppState::get(&self.ui).set_content_active(!focus);
    }

    pub fn go_section(&mut self, section: Section) {
        self.section = section;
        self.rail_index = section.index();
        self.back_stack.clear();
        self.detail = None;
        match section {
            Section::Home | Section::Movies | Section::Shows => {
                self.set_screen(Screen::Browse);
                self.show_browse(section);
            }
            Section::Search => {
                self.set_screen(Screen::Search);
                self.show_search();
            }
            Section::Discover => {
                self.set_screen(Screen::Discover);
                self.show_discover();
            }
            Section::MyList => {
                self.set_screen(Screen::Mylist);
                self.show_mylist();
            }
            Section::Settings => {
                self.set_screen(Screen::Settings);
                self.show_settings();
            }
        }
        self.set_rail(false);
    }

    /// Back from a pushed screen (detail/player).
    pub fn pop_screen(&mut self) {
        match self.back_stack.pop() {
            Some(Screen::Detail) => {
                self.set_screen(Screen::Detail);
                self.refresh_detail_progress();
            }
            Some(Screen::Player) => self.set_screen(Screen::Player),
            Some(other) => {
                self.set_screen(other);
                self.refresh_section();
            }
            None => self.go_section(self.section),
        }
    }

    fn refresh_section(&mut self) {
        match self.section {
            Section::Home | Section::Movies | Section::Shows => self.show_browse(self.section),
            Section::MyList => self.show_mylist(),
            _ => {}
        }
        self.set_rail(false);
    }

    pub fn open(&mut self, target: OpenTarget) {
        if self.screen != Screen::Detail {
            self.back_stack.push(self.screen);
        }
        self.open_detail(target);
    }

    // ---- keys ----

    /// Every key from the UI. Returns false to let the platform handle it
    /// (Back on Home exits the app).
    pub fn on_key(&mut self, raw: &str) -> bool {
        let Some((key, repeat)) = Key::parse(raw) else { return false };
        if self.dialog.is_some() {
            return self.dialog_key(key);
        }
        if self.sources.resolving.is_some() {
            return self.resolve_key(key);
        }
        if self.sources.open {
            return self.sources_key(key);
        }
        match self.screen {
            Screen::Onboarding => self.onboarding_key(key),
            Screen::Player => self.player_key(key, repeat),
            Screen::Detail => self.detail_key(key),
            _ if self.rail_focus => self.rail_key(key),
            Screen::Browse => self.browse_key(key),
            Screen::Search => self.search_key(key),
            Screen::Discover => self.discover_key(key),
            Screen::Mylist => self.mylist_key(key),
            Screen::Settings => self.settings_key(key),
        }
    }

    /// Content moved left past its edge: give the rail focus.
    pub fn focus_rail(&mut self) -> bool {
        self.rail_index = self.section.index();
        self.set_rail(true);
        true
    }

    fn rail_key(&mut self, key: Key) -> bool {
        match key {
            Key::Up => {
                self.rail_index = self.rail_index.saturating_sub(1);
                self.set_rail(true);
            }
            Key::Down => {
                self.rail_index = (self.rail_index + 1).min(SECTIONS.len() - 1);
                self.set_rail(true);
            }
            Key::Right => {
                if SECTIONS[self.rail_index] != self.section {
                    self.go_section(SECTIONS[self.rail_index]);
                } else {
                    self.set_rail(false);
                }
            }
            Key::Ok => self.go_section(SECTIONS[self.rail_index]),
            Key::Back => {
                if self.section != Section::Home {
                    self.go_section(Section::Home);
                    self.focus_rail();
                } else {
                    // Let Android close the app.
                    return false;
                }
            }
            _ => {}
        }
        true
    }

    fn tick(&mut self) {
        let scale = self.ui.window().scale_factor();
        if (scale - self.images.scale()).abs() > 0.01 {
            self.images.set_scale(scale);
        }
        self.player_tick();
        self.resolve_tick();
    }
}

/// Fill a VecModel from a Vec, reusing it.
pub fn set_model<T: Clone + 'static>(model: &VecModel<T>, items: Vec<T>) {
    model.set_vec(items);
}

/// A `Poster` without artwork.
pub fn poster(id: &str, kind: MediaKind, title: &str, subtitle: &str) -> Poster {
    Poster {
        id: id.into(),
        kind: kind.as_str().into(),
        title: title.into(),
        subtitle: subtitle.into(),
        image: slint::Image::default(),
        has_image: false,
        progress: 0.0,
        badge: SharedString::new(),
    }
}

pub fn shared(s: impl AsRef<str>) -> SharedString {
    SharedString::from(s.as_ref())
}

pub fn now_instant() -> Instant {
    Instant::now()
}

pub fn video_title(name: &str, video: &VideoRef, episode_title: Option<&str>) -> (String, String) {
    match (video.episode_label(), episode_title) {
        (Some(label), Some(title)) => (name.to_owned(), format!("{label} · {title}")),
        (Some(label), None) => (name.to_owned(), label),
        _ => (name.to_owned(), String::new()),
    }
}

pub fn browse_state(ui: &AppWindow) -> BrowseState<'_> {
    BrowseState::get(ui)
}

pub fn model_len<T: Clone + 'static>(model: &ModelRc<T>) -> usize {
    model.row_count()
}

pub fn arc<T>(value: T) -> Arc<T> {
    Arc::new(value)
}
