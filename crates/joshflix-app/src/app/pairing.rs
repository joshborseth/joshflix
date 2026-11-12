//! Onboarding and the Real-Debrid key flow (Jedflix-style QR to the phone,
//! or typing with the remote).

use std::time::Duration;

use joshflix_core::CoreError;
use joshflix_core::realdebrid::{RdUser, RealDebrid};
use joshflix_protocol::{CreatePairingResponse, PairingState as Relay};
use slint::{Global, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use tokio::sync::watch;

use super::{Controller, Key, Section, shared, with_controller};
use crate::keyboard::{self, KeyAction, Keyboard};
use crate::{DetailButton, Icons, OnboardingState, PairingState, Screen, SettingsState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingContext {
    Onboarding,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairAction {
    Continue,
    TypeKey,
    NewCode,
}

const LOADING: i32 = 0;
const WAITING: i32 = 1;
const PHONE: i32 = 2;
const CHECKING: i32 = 3;
const SIGNED_IN: i32 = 4;
const ERROR: i32 = 5;

pub struct PairingCtl {
    pub context: PairingContext,
    state: i32,
    status: String,
    account: String,
    url: String,
    qr: Option<Image>,
    cancel: Option<watch::Sender<bool>>,
    typing: bool,
    typed: String,
    keyboard: Keyboard,
    actions: Vec<PairAction>,
    action_index: usize,
    validated: Option<(String, RdUser)>,
    generation: u64,
    leave_timer: slint::Timer,
}

impl PairingCtl {
    pub fn new() -> Self {
        Self {
            context: PairingContext::Onboarding,
            state: LOADING,
            status: String::new(),
            account: String::new(),
            url: String::new(),
            qr: None,
            cancel: None,
            typing: false,
            typed: String::new(),
            keyboard: Keyboard::api_key(),
            actions: Vec::new(),
            action_index: 0,
            validated: None,
            generation: 0,
            leave_timer: slint::Timer::default(),
        }
    }

    pub fn cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(true);
        }
        self.generation += 1;
    }
}

/// Render a QR code as crisp RGBA modules on white.
fn qr_image(text: &str) -> Option<Image> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let width = code.width();
    let quiet = 2;
    let scale = 8;
    let size = (width + quiet * 2) * scale;
    let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(size as u32, size as u32);
    let colors = code.to_colors();
    let buffer = pixels.make_mut_slice();
    for (i, px) in buffer.iter_mut().enumerate() {
        let (x, y) = (i % size / scale, i / size / scale);
        let dark = x >= quiet
            && y >= quiet
            && x < width + quiet
            && y < width + quiet
            && colors[(y - quiet) * width + (x - quiet)] == qrcode::Color::Dark;
        let v = if dark { 0x0b } else { 0xff };
        *px = Rgba8Pixel { r: v, g: v, b: if dark { 0x12 } else { 0xff }, a: 0xff };
    }
    Some(Image::from_rgba8(pixels))
}

impl Controller {
    pub fn show_onboarding(&mut self) {
        self.onboarding_step = 0;
        let state = OnboardingState::get(&self.ui);
        state.set_step(0);
        state.set_leaving(false);
        self.set_screen(Screen::Onboarding);
    }

    pub fn onboarding_key(&mut self, key: Key) -> bool {
        if self.onboarding_step == 0 {
            match key {
                Key::Ok => {
                    OnboardingState::get(&self.ui).set_leaving(true);
                    self.onboarding_step = 1;
                    self.pairing.leave_timer.start(slint::TimerMode::SingleShot, Duration::from_millis(650), || {
                        with_controller(|c| {
                            let state = OnboardingState::get(&c.ui);
                            state.set_step(1);
                            state.set_leaving(false);
                            c.start_pairing(PairingContext::Onboarding);
                        });
                    });
                    true
                }
                // Back on the welcome screen leaves the app.
                Key::Back => false,
                _ => true,
            }
        } else {
            if key == Key::Back && !self.pairing.typing {
                self.pairing.cancel();
                self.show_onboarding();
                return true;
            }
            self.pairing_key(key)
        }
    }

    /// Open the key flow from Settings.
    pub fn open_settings_pairing(&mut self) {
        SettingsState::get(&self.ui).set_pairing_open(true);
        self.start_pairing(PairingContext::Settings);
    }

    fn close_settings_pairing(&mut self) {
        self.pairing.cancel();
        SettingsState::get(&self.ui).set_pairing_open(false);
        self.show_settings();
    }

    pub fn pairing_open(&self) -> bool {
        SettingsState::get(&self.ui).get_pairing_open()
    }

    pub fn start_pairing(&mut self, context: PairingContext) {
        self.pairing.cancel();
        let p = &mut self.pairing;
        p.context = context;
        p.state = LOADING;
        p.status = "Getting a code…".into();
        p.account.clear();
        p.url.clear();
        p.qr = None;
        p.typing = false;
        p.typed.clear();
        p.validated = None;
        p.action_index = 0;
        p.generation += 1;
        let generation = p.generation;
        self.push_pairing();

        let relay = self.svc.relay.clone();
        self.spawn(async move { relay.create_pairing().await }, move |c, result| {
            if c.pairing.generation != generation {
                return;
            }
            match result {
                Ok(pairing) => c.pairing_created(pairing, generation),
                Err(e) => {
                    tracing::warn!("pairing: {e}");
                    c.pairing.state = ERROR;
                    c.pairing.status = "Couldn't reach the pairing server on the dev box. Type the key instead.".into();
                    c.push_pairing();
                }
            }
        });
    }

    fn pairing_created(&mut self, pairing: CreatePairingResponse, generation: u64) {
        let p = &mut self.pairing;
        p.url = pairing.phone_url.clone();
        p.qr = qr_image(&pairing.phone_url);
        p.state = WAITING;
        p.status = "Waiting for your phone…".into();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        p.cancel = Some(cancel_tx);
        self.push_pairing();

        let relay = self.svc.relay.clone();
        self.spawn(
            async move {
                relay
                    .wait_for_key(&pairing, cancel_rx, move |poll| {
                        if poll.state == Relay::PhoneOpened {
                            let _ = slint::invoke_from_event_loop(move || {
                                with_controller(|c| {
                                    if c.pairing.generation == generation && c.pairing.state == WAITING {
                                        c.pairing.state = PHONE;
                                        c.pairing.status = "Phone connected. Paste your key and tap Send to TV.".into();
                                        c.push_pairing();
                                    }
                                })
                            });
                        }
                    })
                    .await
            },
            move |c, result| {
                if c.pairing.generation != generation {
                    return;
                }
                match result {
                    Ok(poll) => match (poll.state, poll.api_key) {
                        (Relay::Delivered, Some(key)) => c.validate_key(key),
                        _ => {
                            // Expired: quietly start a fresh code.
                            let context = c.pairing.context;
                            c.start_pairing(context);
                        }
                    },
                    Err(CoreError::Cancelled) => {}
                    Err(e) => {
                        c.pairing.state = ERROR;
                        c.pairing.status = format!("Lost the pairing server ({e}). Get a new code or type the key.");
                        c.push_pairing();
                    }
                }
            },
        );
    }

    fn validate_key(&mut self, key: String) {
        let key = key.trim().to_owned();
        self.pairing.state = CHECKING;
        self.pairing.status = "Checking your key with Real-Debrid…".into();
        self.pairing.typing = false;
        self.push_pairing();
        let generation = self.pairing.generation;
        let rd = RealDebrid::new(self.svc.http.clone(), key.clone());
        self.spawn(async move { rd.user().await }, move |c, result| {
            if c.pairing.generation != generation {
                return;
            }
            match result {
                Ok(user) if user.is_premium() => {
                    c.pairing.account = format!(
                        "Signed in to Real-Debrid as {} · Premium · {} days left",
                        user.username,
                        user.premium_days_left()
                    );
                    c.pairing.state = SIGNED_IN;
                    c.pairing.status = "Key received".into();
                    c.pairing.validated = Some((key, user));
                    c.pairing.action_index = 0;
                }
                Ok(user) => {
                    c.pairing.state = ERROR;
                    c.pairing.status = format!(
                        "{} doesn't have Real-Debrid premium. Streaming needs an active subscription.",
                        user.username
                    );
                }
                Err(e) => {
                    c.pairing.state = ERROR;
                    c.pairing.status = e.to_string();
                }
            }
            if let Some(cancel) = c.pairing.cancel.take() {
                let _ = cancel.send(true);
            }
            c.push_pairing();
        });
    }

    fn push_pairing(&mut self) {
        let icons = Icons::get(&self.ui);
        let p = &mut self.pairing;
        p.actions = if p.state == SIGNED_IN {
            vec![PairAction::Continue, PairAction::NewCode]
        } else {
            vec![PairAction::TypeKey, PairAction::NewCode]
        };
        p.action_index = p.action_index.min(p.actions.len() - 1);
        let buttons: Vec<DetailButton> = p
            .actions
            .iter()
            .map(|a| match a {
                PairAction::Continue => DetailButton {
                    label: if p.context == PairingContext::Onboarding { "Continue".into() } else { "Save key".into() },
                    icon: icons.get_check(),
                    primary: true,
                },
                PairAction::TypeKey => DetailButton { label: "Type the key with the remote".into(), icon: icons.get_keyboard(), primary: false },
                PairAction::NewCode => DetailButton {
                    label: if p.state == SIGNED_IN { "Use a different key".into() } else { "New QR code".into() },
                    icon: icons.get_refresh(),
                    primary: false,
                },
            })
            .collect();
        let state = PairingState::get(&self.ui);
        state.set_has_qr(p.qr.is_some());
        state.set_qr(p.qr.clone().unwrap_or_default());
        state.set_url(shared(p.url.replace("http://", "")));
        state.set_state(p.state);
        state.set_status(shared(&p.status));
        state.set_account(shared(&p.account));
        state.set_typing(p.typing);
        let masked = if p.typed.len() <= 4 {
            p.typed.clone()
        } else {
            format!("{}{}", "•".repeat(p.typed.len() - 4), &p.typed[p.typed.len() - 4..])
        };
        state.set_typed(shared(masked));
        state.set_keys(p.keyboard.model(&icons));
        state.set_key_index(p.keyboard.index as i32);
        state.set_actions(ModelRc::new(VecModel::from(buttons)));
        state.set_action_index(p.action_index as i32);
    }

    pub fn pairing_key(&mut self, key: Key) -> bool {
        if self.pairing.typing {
            let action = match key {
                Key::Left => {
                    self.pairing.keyboard.step(-1, 0);
                    None
                }
                Key::Right => {
                    self.pairing.keyboard.step(1, 0);
                    None
                }
                Key::Up => {
                    self.pairing.keyboard.step(0, -1);
                    None
                }
                Key::Down => {
                    self.pairing.keyboard.step(0, 1);
                    None
                }
                Key::Ok => Some(self.pairing.keyboard.current().action),
                Key::Back => Some(KeyAction::Cancel),
                Key::Backspace => Some(KeyAction::Backspace),
                Key::Char(c) if c.is_ascii_alphanumeric() => Some(KeyAction::Char(c.to_ascii_uppercase())),
                _ => None,
            };
            match action {
                Some(KeyAction::Done) => {
                    let typed = self.pairing.typed.clone();
                    if typed.len() < 20 {
                        self.pairing.status = "That key looks too short. Real-Debrid keys are 52 characters.".into();
                    } else {
                        self.validate_key(typed);
                        return true;
                    }
                }
                Some(KeyAction::Cancel) => self.pairing.typing = false,
                Some(action) => {
                    keyboard::apply(&mut self.pairing.typed, action, 100);
                }
                None => {}
            }
            self.push_pairing();
            return true;
        }
        match key {
            Key::Up => self.pairing.action_index = self.pairing.action_index.saturating_sub(1),
            Key::Down => self.pairing.action_index = (self.pairing.action_index + 1).min(self.pairing.actions.len() - 1),
            Key::Back => {
                if self.pairing.context == PairingContext::Settings {
                    self.close_settings_pairing();
                    return true;
                }
            }
            Key::Ok => match self.pairing.actions.get(self.pairing.action_index).copied() {
                Some(PairAction::TypeKey) => {
                    self.pairing.typing = true;
                    self.pairing.keyboard = Keyboard::api_key();
                }
                Some(PairAction::NewCode) => {
                    let context = self.pairing.context;
                    self.start_pairing(context);
                    return true;
                }
                Some(PairAction::Continue) => {
                    self.finish_pairing();
                    return true;
                }
                None => {}
            },
            _ => {}
        }
        self.push_pairing();
        true
    }

    fn finish_pairing(&mut self) {
        let Some((key, user)) = self.pairing.validated.take() else { return };
        self.pairing.cancel();
        self.save_settings(|s| {
            s.rd_api_key = Some(key);
            s.rd_username = Some(user.username.clone());
            s.rd_premium_until = user.expiration.clone();
            s.onboarding_done = true;
        });
        match self.pairing.context {
            PairingContext::Onboarding => {
                self.toast(&format!("Welcome, {}!", user.username));
                self.go_section(Section::Home);
                self.check_for_update(false);
            }
            PairingContext::Settings => {
                self.toast("Real-Debrid key saved");
                self.close_settings_pairing();
            }
        }
    }

    /// Check the saved key in the background and refresh the cached account.
    pub fn refresh_account_silently(&mut self) {
        let Some(key) = self.rd_key() else { return };
        let rd = RealDebrid::new(self.svc.http.clone(), key);
        self.spawn(async move { rd.user().await }, |c, result| match result {
            Ok(user) => {
                c.save_settings(|s| {
                    s.rd_username = Some(user.username.clone());
                    s.rd_premium_until = user.expiration.clone();
                });
                if !user.is_premium() {
                    c.toast("Your Real-Debrid premium has expired. Streams won't play until it's renewed.");
                }
                if c.screen == Screen::Settings {
                    c.show_settings();
                }
            }
            Err(CoreError::RealDebrid(e)) if e.kind == joshflix_core::realdebrid::RdErrorKind::BadKey => {
                c.toast("Your Real-Debrid key stopped working. Add it again in Settings → Real-Debrid.");
            }
            Err(e) => tracing::info!("account refresh: {e}"),
        });
    }
}
