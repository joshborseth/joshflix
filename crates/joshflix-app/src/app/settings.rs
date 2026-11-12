//! Settings: Real-Debrid, playback, subtitles, sources and About/updates.

use joshflix_core::release::Resolution;
use joshflix_core::subtitles::LANGUAGES;
use slint::{Global, ModelRc, VecModel};
use tokio::sync::watch;

use super::dialog::DialogAction;
use super::{Controller, Key, shared};
use crate::platform::current_abi;
use crate::{Icons, NavItem, SettingItem, SettingsState};

pub const VERSION_NAME: &str = env!("CARGO_PKG_VERSION");
pub const VERSION_CODE: u64 = match option_env!("JOSHFLIX_VERSION_CODE") {
    Some(code) => parse_u64(code),
    None => 1,
};

const fn parse_u64(s: &str) -> u64 {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut value = 0u64;
    while i < bytes.len() {
        value = value * 10 + (bytes[i] - b'0') as u64;
        i += 1;
    }
    value
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    None,
    ChangeKey,
    RemoveKey,
    AutoplayNext,
    FrameRate,
    NightMode,
    VideoDecoding,
    SubtitlesOn,
    SubtitleLanguage,
    SubtitleSize,
    HideJunk,
    MatchDisplay,
    MaxResolution,
    MaxSize,
    CheckUpdate,
}

struct Item {
    kind: &'static str,
    label: String,
    value: String,
    on: bool,
    hint: String,
    action: Action,
}

impl Item {
    fn info(label: &str, value: impl Into<String>) -> Self {
        Self { kind: "info", label: label.into(), value: value.into(), on: false, hint: String::new(), action: Action::None }
    }
    fn toggle(label: &str, on: bool, hint: &str, action: Action) -> Self {
        Self { kind: "toggle", label: label.into(), value: String::new(), on, hint: hint.into(), action }
    }
    fn choice(label: &str, value: impl Into<String>, hint: &str, action: Action) -> Self {
        Self { kind: "choice", label: label.into(), value: value.into(), on: false, hint: hint.into(), action }
    }
    fn action(label: &str, value: impl Into<String>, hint: &str, action: Action) -> Self {
        Self { kind: "action", label: label.into(), value: value.into(), on: false, hint: hint.into(), action }
    }
}

pub struct SettingsCtl {
    section: usize,
    zone: i32,
    index: usize,
    items: Vec<Item>,
    update_status: String,
    downloading: Option<watch::Sender<bool>>,
}

impl SettingsCtl {
    pub fn new() -> Self {
        Self { section: 0, zone: 0, index: 0, items: Vec::new(), update_status: String::new(), downloading: None }
    }
}

const SECTIONS: [(&str, &str); 5] = [
    ("Real-Debrid", "Your Real-Debrid account powers every stream."),
    ("Playback", "How episodes and the player behave."),
    ("Subtitles", "Subtitles come from the file itself and from OpenSubtitles."),
    ("Sources", "Which Comet results appear when you press Play."),
    ("About", ""),
];

const SIZES: [(f32, &str); 4] = [(0.8, "Small"), (1.0, "Medium"), (1.25, "Large"), (1.5, "Extra large")];
const MAX_SIZES: [Option<f32>; 6] = [None, Some(5.0), Some(10.0), Some(20.0), Some(40.0), Some(60.0)];
const MAX_RES: [Option<Resolution>; 4] = [None, Some(Resolution::R2160), Some(Resolution::R1080), Some(Resolution::R720)];

fn caps_label(caps: &joshflix_core::release::DisplayCaps) -> String {
    let mut parts = vec![if caps.max_height >= 2160 { "4K".to_owned() } else { format!("{}p", caps.max_height) }];
    if caps.dolby_vision {
        parts.push("Dolby Vision".into());
    }
    if caps.hdr10_plus {
        parts.push("HDR10+".into());
    } else if caps.hdr10 {
        parts.push("HDR10".into());
    }
    if caps.hlg {
        parts.push("HLG".into());
    }
    if !caps.hdr10 && !caps.dolby_vision {
        parts.push("SDR".into());
    }
    parts.join(" · ")
}

impl Controller {
    pub fn show_settings(&mut self) {
        let icons = Icons::get(&self.ui);
        let sections: Vec<NavItem> = [
            ("Real-Debrid", icons.get_bolt()),
            ("Playback", icons.get_play()),
            ("Subtitles", icons.get_subtitles()),
            ("Sources", icons.get_filter()),
            ("About", icons.get_info()),
        ]
        .into_iter()
        .map(|(label, icon)| NavItem { label: label.into(), icon })
        .collect();
        SettingsState::get(&self.ui).set_sections(ModelRc::new(VecModel::from(sections)));
        self.build_settings();
    }

    fn build_settings(&mut self) {
        let s = &self.settings;
        let items = match self.settings_ctl.section {
            0 => vec![
                Item::info("Account", s.rd_username.clone().unwrap_or_else(|| "Not signed in".into())),
                Item::info(
                    "Premium until",
                    s.rd_premium_until.as_deref().and_then(crate::format::air_date).unwrap_or_else(|| "Unknown".into()),
                ),
                Item::action("Change key", "", "Scan a QR code with your phone or type a new key", Action::ChangeKey),
                Item { kind: "danger", ..Item::action("Remove key", "", "Signs this TV out of Real-Debrid", Action::RemoveKey) },
            ],
            1 => vec![
                Item::toggle("Autoplay next episode", s.autoplay_next, "Counts down 10 seconds in the last minute", Action::AutoplayNext),
                Item::toggle("Match frame rate", s.frame_rate_matching, "Switches the TV to the video's frame rate (24p…) when supported", Action::FrameRate),
                Item::toggle("Night mode by default", s.night_mode, "Quieter loud scenes, clearer dialogue. Toggle anytime in the player", Action::NightMode),
                Item::choice(
                    "Video decoding",
                    match s.video_compat_mode {
                        None => "Automatic",
                        Some(false) => "Hardware",
                        Some(true) => "Compatibility",
                    },
                    "Compatibility mode helps TVs that show a black screen. Applies after restarting Joshflix",
                    Action::VideoDecoding,
                ),
            ],
            2 => {
                let lang = s.subtitle_languages.first().map(|c| joshflix_core::subtitles::language_name(c)).unwrap_or("English");
                let size = SIZES.iter().min_by(|a, b| (a.0 - s.subtitle_scale).abs().total_cmp(&(b.0 - s.subtitle_scale).abs())).map_or("Medium", |s| s.1);
                vec![
                    Item::toggle("Turn on subtitles automatically", s.subtitles_enabled, "Picks your language when it's available", Action::SubtitlesOn),
                    Item::choice("Language", lang, "Also used to fetch OpenSubtitles", Action::SubtitleLanguage),
                    Item::choice("Size", size, "", Action::SubtitleSize),
                ]
            }
            3 => {
                let f = s.filters;
                let res = match f.max_resolution {
                    None => "Any".to_owned(),
                    Some(r) => r.label().to_owned(),
                };
                let size = f.max_size_gb.map_or("No limit".to_owned(), |g| format!("{g:.0} GB"));
                vec![
                    Item::toggle("Hide CAM and screener copies", f.hide_junk, "Filters out theater recordings", Action::HideJunk),
                    Item::toggle("Match my TV", f.match_display, &format!("This TV: {}", caps_label(&self.caps)), Action::MatchDisplay),
                    Item::choice("Highest resolution", res, "", Action::MaxResolution),
                    Item::choice("Largest file", size, "", Action::MaxSize),
                ]
            }
            _ => vec![
                Item::info("Version", format!("{VERSION_NAME} ({VERSION_CODE}) · {}", current_abi())),
                Item::action(
                    "Check for updates",
                    self.settings_ctl.update_status.clone(),
                    "Updates come from the private server on the dev box",
                    Action::CheckUpdate,
                ),
                Item::info("Device", self.platform.device_label()),
                Item::info("Sources", "Comet (ElfHosted) · Cinemeta · OpenSubtitles"),
                Item::info("Pairing server", self.svc.relay.base().replace("http://", "")),
            ],
        };
        self.settings_ctl.items = items;
        self.settings_ctl.index = self.settings_ctl.index.min(self.settings_ctl.items.len().saturating_sub(1));
        self.push_settings();
    }

    fn push_settings(&self) {
        let c = &self.settings_ctl;
        let state = SettingsState::get(&self.ui);
        let items: Vec<SettingItem> = c
            .items
            .iter()
            .map(|i| SettingItem {
                kind: i.kind.into(),
                label: shared(&i.label),
                value: shared(&i.value),
                on: i.on,
                hint: shared(&i.hint),
            })
            .collect();
        state.set_items(ModelRc::new(VecModel::from(items)));
        state.set_section_index(c.section as i32);
        state.set_item_index(c.index as i32);
        state.set_zone(c.zone);
        let (title, blurb) = SECTIONS[c.section];
        state.set_section_title(title.into());
        state.set_section_blurb(blurb.into());
    }

    pub fn settings_key(&mut self, key: Key) -> bool {
        if self.pairing_open() {
            return self.pairing_key(key);
        }
        let c = &mut self.settings_ctl;
        match (c.zone, key) {
            (_, Key::Back) => {
                if c.zone == 1 {
                    c.zone = 0;
                } else {
                    return self.focus_rail();
                }
            }
            (0, Key::Up) => {
                c.section = c.section.saturating_sub(1);
                c.index = 0;
                self.build_settings();
                return true;
            }
            (0, Key::Down) => {
                c.section = (c.section + 1).min(SECTIONS.len() - 1);
                c.index = 0;
                self.build_settings();
                return true;
            }
            (0, Key::Left) => return self.focus_rail(),
            (0, Key::Right) | (0, Key::Ok) => c.zone = 1,
            (1, Key::Up) => c.index = c.index.saturating_sub(1),
            (1, Key::Down) => c.index = (c.index + 1).min(c.items.len().saturating_sub(1)),
            (1, Key::Left) | (1, Key::Right) | (1, Key::Ok) => {
                let item = &c.items[c.index];
                let step: i32 = if key == Key::Left { -1 } else { 1 };
                if key == Key::Left && item.kind != "choice" {
                    c.zone = 0;
                } else {
                    let action = item.action;
                    self.settings_action(action, step, key == Key::Ok);
                    return true;
                }
            }
            _ => {}
        }
        self.push_settings();
        true
    }

    fn settings_action(&mut self, action: Action, step: i32, ok: bool) {
        fn cycle<T: PartialEq + Copy>(options: &[T], current: T, step: i32) -> T {
            let i = options.iter().position(|o| *o == current).unwrap_or(0) as i32;
            options[(i + step).rem_euclid(options.len() as i32) as usize]
        }
        match action {
            Action::None => return,
            Action::ChangeKey if ok => {
                self.open_settings_pairing();
                return;
            }
            Action::RemoveKey if ok => {
                self.confirm_remove_key();
                return;
            }
            Action::CheckUpdate if ok => {
                self.check_for_update(true);
                return;
            }
            Action::AutoplayNext => self.save_settings(|s| s.autoplay_next = !s.autoplay_next),
            Action::FrameRate => self.save_settings(|s| s.frame_rate_matching = !s.frame_rate_matching),
            Action::NightMode => self.save_settings(|s| s.night_mode = !s.night_mode),
            Action::VideoDecoding => {
                let next = cycle(&[None, Some(false), Some(true)], self.settings.video_compat_mode, step);
                self.save_settings(|s| s.video_compat_mode = next);
            }
            Action::SubtitlesOn => self.save_settings(|s| s.subtitles_enabled = !s.subtitles_enabled),
            Action::HideJunk => self.save_settings(|s| s.filters.hide_junk = !s.filters.hide_junk),
            Action::MatchDisplay => self.save_settings(|s| s.filters.match_display = !s.filters.match_display),
            Action::SubtitleLanguage => {
                let codes: Vec<&str> = LANGUAGES.iter().map(|(c, _)| *c).collect();
                let current = self.settings.subtitle_languages.first().cloned().unwrap_or_else(|| "eng".into());
                let next = cycle(&codes, current.as_str(), step).to_owned();
                self.save_settings(|s| s.subtitle_languages = vec![next]);
            }
            Action::SubtitleSize => {
                let scales: Vec<u32> = SIZES.iter().map(|s| (s.0 * 100.0) as u32).collect();
                let current = (self.settings.subtitle_scale * 100.0).round() as u32;
                let current = *scales.iter().min_by_key(|s| s.abs_diff(current)).unwrap_or(&100);
                let next = cycle(&scales, current, step) as f32 / 100.0;
                self.save_settings(|s| s.subtitle_scale = next);
            }
            Action::MaxResolution => {
                let next = cycle(&MAX_RES, self.settings.filters.max_resolution, step);
                self.save_settings(|s| s.filters.max_resolution = next);
            }
            Action::MaxSize => {
                let current = MAX_SIZES.iter().position(|m| *m == self.settings.filters.max_size_gb).unwrap_or(0);
                let next = MAX_SIZES[(current as i32 + step).rem_euclid(MAX_SIZES.len() as i32) as usize];
                self.save_settings(|s| s.filters.max_size_gb = next);
            }
            _ => return,
        }
        self.build_settings();
    }

    // ---- updates ----

    pub fn check_for_update(&mut self, manual: bool) {
        if manual {
            self.settings_ctl.update_status = "Checking…".into();
            self.build_settings();
        }
        let updater = self.svc.updater.clone();
        self.spawn(async move { updater.check(VERSION_CODE, &[current_abi()]).await }, move |c, result| {
            match result {
                Ok(Some(update)) => {
                    let code = update.manifest.version_code;
                    let name = update.manifest.version_name.clone();
                    c.settings_ctl.update_status = format!("{name} available");
                    let notes = update.manifest.notes.clone();
                    c.update_available = Some(update);
                    if manual || c.settings.dismissed_update_code != Some(code) {
                        let icons = Icons::get(&c.ui);
                        let body = if notes.trim().is_empty() {
                            format!("Joshflix {name} is ready to install. You're on {VERSION_NAME}.")
                        } else {
                            notes
                        };
                        c.show_dialog(
                            &format!("Update to Joshflix {name}?"),
                            &body,
                            vec![
                                ("Update now", icons.get_download(), true, DialogAction::InstallUpdate),
                                ("Later", icons.get_close(), false, DialogAction::DismissUpdate),
                            ],
                        );
                    }
                }
                Ok(None) => {
                    c.settings_ctl.update_status = "Up to date".into();
                    if manual {
                        c.toast("You're on the latest version.");
                    }
                }
                Err(e) => {
                    c.settings_ctl.update_status = "Server unreachable".into();
                    if manual {
                        c.toast(&format!("Couldn't check for updates: {e}"));
                    }
                }
            }
            if c.screen == crate::Screen::Settings {
                c.build_settings();
            }
        });
    }

    pub fn install_update(&mut self) {
        let Some(update) = self.update_available.clone() else { return };
        if !self.platform.can_install_updates() {
            self.toast("Updates install on the TV. On desktop, rebuild instead.");
            return;
        }
        let (cancel_tx, cancel_rx) = watch::channel(false);
        self.settings_ctl.downloading = Some(cancel_tx);
        self.settings_ctl.update_status = "Downloading…".into();
        let state = SettingsState::get(&self.ui);
        state.set_busy_progress(0.0);
        let updater = self.svc.updater.clone();
        let dir = self.platform.cache_dir().join("updates");
        let progress = move |fraction: f32| {
            let _ = slint::invoke_from_event_loop(move || {
                super::with_controller(|c| SettingsState::get(&c.ui).set_busy_progress(fraction));
            });
        };
        self.toast("Downloading the update…");
        self.spawn(async move { updater.download(&update, &dir, progress, cancel_rx).await }, |c, result| {
            SettingsState::get(&c.ui).set_busy_progress(-1.0);
            c.settings_ctl.downloading = None;
            match result.map_err(|e| e.to_string()).and_then(|path| c.platform.install_apk(&path)) {
                Ok(()) => c.settings_ctl.update_status = "Installing…".into(),
                Err(e) => {
                    c.settings_ctl.update_status = "Update failed".into();
                    c.toast(&e);
                }
            }
            if c.screen == crate::Screen::Settings {
                c.build_settings();
            }
        });
    }
}
