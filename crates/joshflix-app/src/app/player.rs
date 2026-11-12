//! Playback: overlay, seeking, tracks, night mode, progress and next episode.

use std::time::{Duration, Instant};

use joshflix_core::VideoRef;
use joshflix_core::sources::{RankedSources, Source, pick_for_binge};
use joshflix_core::store::{LastSource, Progress};
use joshflix_player::{EndReason, PlayerEvent, TrackKind};
use slint::{Global, ModelRc, VecModel};

use super::detail::{PlayContext, PlayRequest};
use super::{Controller, Key, shared};
use crate::format;
use crate::{PlayerState, Screen, TrackItem};

const OVERLAY_SECS: u64 = 5;
const SAVE_EVERY: Duration = Duration::from_secs(10);
const NEXT_CARD_SECS: f64 = 60.0;
const COUNTDOWN_SECS: u64 = 10;

const CONTROL_COUNT_WITH_NEXT: usize = 7;
const CONTROL_COUNT: usize = 6;

struct TrackMenu {
    kind: TrackKind,
    entries: Vec<(Option<i64>, String, bool)>,
    index: usize,
}

struct NextCtl {
    request: Option<PlayRequest>,
    header: String,
    title: String,
    countdown_from: Option<Instant>,
    focused: bool,
    dismissed: bool,
    shown: bool,
}

pub struct PlayerCtl {
    pub request: PlayRequest,
    pub source: Source,
    /// The direct stream URL (kept so playback can be reloaded).
    url: String,
    position: f64,
    duration: f64,
    paused: bool,
    buffering: Option<i64>,
    pub video_shown: bool,
    overlay_until: Instant,
    zone: i32,
    control: usize,
    seek_target: Option<f64>,
    seek_commit_at: Option<Instant>,
    menu: Option<TrackMenu>,
    next: NextCtl,
    last_save: Instant,
    finished: bool,
    error: Option<String>,
    subtitles: Vec<joshflix_core::subtitles::Subtitle>,
    subtitles_added: bool,
    /// Pick the subtitle track once the track list (incl. added subs) is known.
    subtitle_choice_pending: bool,
    subtitles_on: bool,
    /// When the file loaded, to detect a stalled zero-copy decoder.
    loaded_at: Option<Instant>,
    /// mpv reported PLAYBACK_RESTART (the first frame is up) for this file.
    started: bool,
    hwdec_fallback: bool,
    /// Last overlay visibility pushed to mpv (to lift subtitles above the controls).
    overlay_shown: Option<bool>,
    night: bool,
}

impl PlayerCtl {
    fn controls(&self) -> usize {
        if self.next.request.is_some() { CONTROL_COUNT_WITH_NEXT } else { CONTROL_COUNT }
    }
}

impl Controller {
    pub fn start_playback(&mut self, request: PlayRequest, source: Source, url: String) {
        let Some(player) = self.video.player() else {
            let reason = self.video.unavailable_reason().unwrap_or_default();
            self.toast(&format!("The video player isn't available. {reason}"));
            return;
        };
        // Replace whatever is playing (next episode) without leaving the player.
        if self.player.is_some() {
            self.stop_playback(true);
        } else if self.screen != Screen::Player {
            self.back_stack.push(self.screen);
        }
        let night = self.settings.night_mode;
        let _ = player.set_night_mode(night);
        let _ = player.set_subtitle_scale(f64::from(self.settings.subtitle_scale));
        if let Err(e) = player.load(&url, request.resume_at) {
            self.toast(&e.to_string());
            return;
        }
        self.platform.keep_screen_on(true);
        let next = self.next_for(&request);
        self.player = Some(PlayerCtl {
            request: request.clone(),
            source,
            url: url.clone(),
            position: request.resume_at.unwrap_or(0.0),
            duration: 0.0,
            paused: false,
            buffering: Some(0),
            video_shown: false,
            overlay_until: Instant::now() + Duration::from_secs(OVERLAY_SECS),
            zone: 0,
            control: 0,
            seek_target: None,
            seek_commit_at: None,
            menu: None,
            next,
            last_save: Instant::now(),
            finished: false,
            error: None,
            subtitles: Vec::new(),
            subtitles_added: false,
            subtitle_choice_pending: false,
            subtitles_on: false,
            loaded_at: None,
            started: false,
            hwdec_fallback: false,
            overlay_shown: None,
            night,
        });
        let (title, subtitle) = super::video_title(&request.context.name, &request.video, request.context.episode_title.as_deref());
        let state = PlayerState::get(&self.ui);
        state.set_title(shared(title));
        state.set_subtitle(shared(subtitle));
        state.set_error("".into());
        state.set_menu_open(false);
        state.set_next_visible(false);
        state.set_night_mode(night);
        state.set_buffering(true);
        state.set_buffering_text("Loading…".into());
        self.set_screen(Screen::Player);
        self.push_player();

        // Subtitle list from OpenSubtitles (added after the file loads).
        let langs = self.settings.subtitle_languages.clone();
        let subs = self.svc.subtitles.clone();
        let video = request.video.clone();
        self.spawn(async move { subs.find(&video, &langs, 3).await.unwrap_or_default() }, move |c, found| {
            if let Some(p) = c.player.as_mut() {
                p.subtitles = found;
                if p.video_shown {
                    c.add_subtitles();
                }
            }
        });
    }

    /// Work out the next episode (and the card's wording) for a series.
    fn next_for(&self, request: &PlayRequest) -> NextCtl {
        let empty = NextCtl { request: None, header: String::new(), title: String::new(), countdown_from: None, focused: false, dismissed: false, shown: false };
        let (Some(detail), Some(season), Some(episode)) = (request.context.detail.as_deref(), request.video.season, request.video.episode) else {
            return empty;
        };
        let Some(next) = detail.next_episode(season, episode) else {
            return NextCtl { header: "FINAL EPISODE".into(), title: format!("That's the end of {}", detail.preview.name), ..empty };
        };
        let header = if detail.is_season_finale(season, episode) {
            format!("SEASON {season} FINALE  ·  UP NEXT")
        } else {
            "NEXT EPISODE".into()
        };
        let video = VideoRef::episode(&request.video.meta_id, next.season, next.episode);
        let title = format!("{} · {}", video.episode_label().unwrap_or_default(), next.title);
        NextCtl {
            request: Some(PlayRequest {
                video,
                context: PlayContext {
                    episode_title: Some(next.title.clone()),
                    thumbnail: next.thumbnail.clone(),
                    ..request.context.clone()
                },
                resume_at: None,
                preferred: None,
                fallback_to_picker: true,
            }),
            header,
            title,
            ..empty
        }
    }

    pub fn on_player_event(&mut self, event: PlayerEvent) {
        if let PlayerEvent::Log { level, text } = &event {
            match level.as_str() {
                "error" | "fatal" => tracing::error!("mpv: {text}"),
                "warn" => tracing::warn!("mpv: {text}"),
                _ => tracing::info!("mpv[{level}]: {text}"),
            }
            return;
        }
        let Some(p) = self.player.as_mut() else { return };
        match event {
            PlayerEvent::FileLoaded => {
                p.video_shown = true;
                p.subtitle_choice_pending = true;
                p.loaded_at = Some(Instant::now());
                self.set_screen(Screen::Player);
                self.add_subtitles();
            }
            PlayerEvent::Started => {
                p.buffering = None;
                p.started = true;
            }
            PlayerEvent::Position(pos) => p.position = pos,
            PlayerEvent::Duration(d) => p.duration = d,
            PlayerEvent::Paused(paused) => {
                p.paused = paused;
                if paused {
                    p.overlay_until = Instant::now() + Duration::from_secs(3600);
                    self.save_progress(false);
                } else {
                    let p = self.player.as_mut().expect("player");
                    p.overlay_until = Instant::now() + Duration::from_secs(OVERLAY_SECS);
                }
            }
            PlayerEvent::Buffering(state) => p.buffering = state,
            PlayerEvent::TracksChanged => {
                self.choose_subtitle();
                self.refresh_subtitle_state();
            }
            PlayerEvent::FrameRate(fps) => {
                if self.settings.frame_rate_matching && fps > 1.0 {
                    self.platform.set_frame_rate(fps);
                }
            }
            PlayerEvent::Ended(EndReason::Eof) => {
                if !p.finished {
                    p.finished = true;
                    self.on_finished();
                }
                return;
            }
            PlayerEvent::Ended(EndReason::Error(message)) => {
                p.error = Some(format!("Playback failed: {message}. Press Back and try another source."));
                p.buffering = None;
            }
            _ => {}
        }
        self.push_player();
    }

    fn add_subtitles(&mut self) {
        let Some(p) = self.player.as_mut() else { return };
        if p.subtitles_added || p.subtitles.is_empty() {
            return;
        }
        p.subtitles_added = true;
        // Re-evaluate the choice once these arrive (they can land after the file loaded).
        p.subtitle_choice_pending = true;
        let Some(player) = self.video.player() else { return };
        let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for sub in &p.subtitles {
            let n = counts.entry(sub.lang.as_str()).or_default();
            *n += 1;
            let _ = player.add_subtitle(&sub.url, &format!("OpenSubtitles {n}"), &sub.lang);
        }
        // The choice happens in `choose_subtitle` once mpv reports the new tracks.
    }

    /// Apply the subtitle preference once the track list includes the subtitles we added.
    fn choose_subtitle(&mut self) {
        let Some(p) = self.player.as_mut() else { return };
        if !p.subtitle_choice_pending {
            return;
        }
        let Some(player) = self.video.player() else { return };
        let tracks = player.tracks();
        if tracks.is_empty() {
            return;
        }
        let expected = if p.subtitles_added { p.subtitles.len() } else { 0 };
        let external = tracks.iter().filter(|t| t.kind == TrackKind::Subtitle && t.external).count();
        if external < expected {
            return; // more OpenSubtitles tracks still loading
        }
        p.subtitle_choice_pending = false;
        let wanted = self.settings.subtitles_enabled;
        let preferred = self.settings.subtitle_languages.first().cloned().unwrap_or_default();
        let matches = |t: &&joshflix_player::Track| {
            t.kind == TrackKind::Subtitle
                && t.lang.as_deref().is_some_and(|l| joshflix_player::language_label(l) == joshflix_core::subtitles::language_name(&preferred))
        };
        let choice = if wanted {
            tracks
                .iter()
                .filter(matches)
                .find(|t| !t.external && !t.forced)
                .or_else(|| tracks.iter().filter(matches).next())
                .map(|t| t.id)
        } else {
            // Off, but keep forced subtitles (foreign-language lines) in the preferred language.
            tracks.iter().filter(matches).find(|t| t.forced).map(|t| t.id)
        };
        let _ = player.select_track(TrackKind::Subtitle, choice);
    }

    /// Whether a subtitle track is showing (cheap enough to call on track changes only).
    fn refresh_subtitle_state(&mut self) {
        let on = self
            .video
            .player()
            .is_some_and(|pl| pl.tracks().iter().any(|t| t.kind == TrackKind::Subtitle && t.selected));
        if let Some(p) = self.player.as_mut() {
            p.subtitles_on = on;
        }
    }

    fn push_player(&mut self) {
        let Some(p) = self.player.as_mut() else { return };
        let overlay = Instant::now() < p.overlay_until || p.paused || p.menu.is_some() || p.seek_target.is_some() || p.error.is_some();
        if p.overlay_shown != Some(overlay) {
            p.overlay_shown = Some(overlay);
            if let Some(player) = self.video.player() {
                // Lift subtitles above the seek bar while the controls show.
                let _ = player.set_property("sub-margin-y", if overlay { "150" } else { "48" });
            }
        }
        let p = self.player.as_ref().expect("player");
        let state = PlayerState::get(&self.ui);
        state.set_overlay_visible(overlay);
        state.set_paused(p.paused);
        let buffering = p.buffering.is_some() && p.error.is_none();
        state.set_buffering(buffering);
        state.set_buffering_text(shared(match p.buffering {
            Some(pct) if pct > 0 => format!("Buffering {pct}%"),
            _ if !p.video_shown => "Loading…".to_owned(),
            _ => "Buffering…".to_owned(),
        }));
        let progress = if p.duration > 0.0 { (p.position / p.duration) as f32 } else { 0.0 };
        state.set_progress(progress);
        state.set_position_text(shared(format::clock(p.position)));
        state.set_duration_text(shared(format::clock(p.duration)));
        state.set_remaining_text(shared(if p.duration > 0.0 { format!("-{}", format::clock(p.duration - p.position)) } else { String::new() }));
        if let Some(target) = p.seek_target {
            state.set_seeking(true);
            state.set_seek_progress(if p.duration > 0.0 { (target / p.duration) as f32 } else { 0.0 });
            state.set_seek_text(shared(format::clock(target)));
        } else {
            state.set_seeking(false);
        }
        state.set_zone(p.zone);
        state.set_control_index(p.control as i32);
        state.set_night_mode(p.night);
        state.set_has_next(p.next.request.is_some());
        state.set_subtitles_on(p.subtitles_on);
        match &p.menu {
            Some(menu) => {
                state.set_menu_open(true);
                state.set_menu_title(if menu.kind == TrackKind::Audio { "Audio".into() } else { "Subtitles".into() });
                let options: Vec<TrackItem> = menu
                    .entries
                    .iter()
                    .map(|(_, label, selected)| TrackItem { label: shared(label), selected: *selected })
                    .collect();
                state.set_menu_options(ModelRc::new(VecModel::from(options)));
                state.set_menu_index(menu.index as i32);
            }
            None => state.set_menu_open(false),
        }
        state.set_next_visible(p.next.shown && !p.next.dismissed);
        state.set_next_header(shared(&p.next.header));
        state.set_next_title(shared(&p.next.title));
        state.set_next_focused(p.next.focused);
        state.set_next_countdown(match p.next.countdown_from {
            Some(from) => COUNTDOWN_SECS.saturating_sub(from.elapsed().as_secs()) as i32,
            None => -1,
        });
        state.set_error(shared(p.error.as_deref().unwrap_or_default()));
    }

    pub(super) fn player_tick(&mut self) {
        let Some(p) = self.player.as_mut() else { return };
        // Some TV/emulator drivers accept MediaCodec's zero-copy output but never
        // deliver frames. If nothing has played 6 s after loading, switch to the
        // copy-back path, which works everywhere (mpv reinitializes the decoder).
        if cfg!(target_os = "android")
            && !p.hwdec_fallback
            && !p.paused
            && !p.started
            && p.loaded_at.is_some_and(|t| t.elapsed() > Duration::from_secs(8))
        {
            p.hwdec_fallback = true;
            tracing::warn!("no frames from zero-copy MediaCodec; switching to compatibility mode");
            self.recover_with_compat_mode();
            return;
        }
        // Commit a pending seek once the user pauses on a target.
        if let (Some(target), Some(at)) = (p.seek_target, p.seek_commit_at)
            && Instant::now() >= at
        {
            p.seek_target = None;
            p.seek_commit_at = None;
            p.position = target;
            if let Some(player) = self.video.player() {
                let _ = player.seek_to(target);
            }
        }
        // Next-episode card in the last minute.
        let p = self.player.as_mut().expect("player");
        let remaining = p.duration - p.position;
        let near_end = p.duration > 120.0 && remaining <= NEXT_CARD_SECS && remaining > 0.0;
        if near_end && !p.next.dismissed && !p.next.shown && !p.next.header.is_empty() {
            p.next.shown = true;
            if p.next.request.is_some() && self.settings.autoplay_next {
                p.next.countdown_from = Some(Instant::now());
            }
        }
        if !near_end && p.next.shown && remaining > NEXT_CARD_SECS {
            // Seeked back out of the credits.
            p.next.shown = false;
            p.next.countdown_from = None;
            p.next.focused = false;
        }
        let countdown_done = p
            .next
            .countdown_from
            .is_some_and(|from| from.elapsed() >= Duration::from_secs(COUNTDOWN_SECS) && !p.paused);
        let save_due = p.video_shown && p.last_save.elapsed() >= SAVE_EVERY && !p.paused;
        if countdown_done {
            self.play_next();
            return;
        }
        if save_due {
            self.save_progress(false);
        }
        self.push_player();
    }

    fn save_progress(&mut self, finished: bool) {
        let Some(p) = self.player.as_mut() else { return };
        if !p.video_shown || p.duration <= 0.0 {
            return;
        }
        p.last_save = Instant::now();
        let r = &p.request;
        let progress = Progress {
            video: r.video.clone(),
            name: r.context.name.clone(),
            episode_title: r.context.episode_title.clone(),
            poster: r.context.poster.clone(),
            background: r.context.background.clone(),
            thumbnail: r.context.thumbnail.clone(),
            logo: r.context.logo.clone(),
            position_secs: if finished { p.duration } else { p.position },
            duration_secs: p.duration,
            updated_at: 0,
            up_next: false,
            last_source: Some(LastSource { stream: p.source.clone() }),
        };
        let watched = progress.is_watched();
        if let Err(e) = self.svc.store.save_progress(progress) {
            tracing::warn!("save progress: {e}");
        }
        // Queue the next episode for Continue Watching.
        if watched
            && let Some(next) = p.next.request.as_ref()
            && self.svc.store.progress(&next.video).ok().flatten().is_none_or(|existing| existing.up_next)
        {
            std::thread::sleep(Duration::from_millis(2)); // keep it newer than the finished one
            let _ = self.svc.store.save_progress(Progress {
                video: next.video.clone(),
                name: next.context.name.clone(),
                episode_title: next.context.episode_title.clone(),
                poster: next.context.poster.clone(),
                background: next.context.background.clone(),
                thumbnail: next.context.thumbnail.clone(),
                logo: next.context.logo.clone(),
                position_secs: 0.0,
                duration_secs: 0.0,
                updated_at: 0,
                up_next: true,
                last_source: None,
            });
        }
    }

    fn on_finished(&mut self) {
        self.save_progress(true);
        let autoplay = self.settings.autoplay_next;
        let has_next = self.player.as_ref().is_some_and(|p| p.next.request.is_some() && !p.next.dismissed);
        if autoplay && has_next {
            self.play_next();
        } else {
            self.exit_player();
        }
    }

    /// Stop mpv and forget the session.
    pub fn stop_playback(&mut self, save: bool) {
        if save {
            self.save_progress(false);
        }
        if let Some(player) = self.video.player() {
            let _ = player.stop();
        }
        if self.player.take().is_some() {
            self.platform.keep_screen_on(false);
            if self.settings.frame_rate_matching {
                self.platform.set_frame_rate(0.0);
            }
        }
    }

    fn exit_player(&mut self) {
        self.stop_playback(true);
        self.pop_screen();
    }

    fn play_next(&mut self) {
        let Some(p) = self.player.as_mut() else { return };
        let Some(request) = p.next.request.clone() else { return };
        p.next.countdown_from = None;
        let previous = p.source.clone();
        self.save_progress(true);
        self.autoplay(request, previous);
    }

    /// Next episode: pick the same release family as before and play it, or
    /// fall back to the picker.
    fn autoplay(&mut self, request: PlayRequest, previous: Source) {
        let Some(comet) = self.comet() else { return };
        let video = request.video.clone();
        let (filters, caps) = (self.settings.filters, self.caps);
        if let Some(p) = self.player.as_mut() {
            p.buffering = Some(0);
            p.error = None;
        }
        PlayerState::get(&self.ui).set_buffering_text("Finding the next episode…".into());
        self.spawn(async move { comet.streams(&video).await }, move |c, result| {
            let pick = result.ok().and_then(|r| {
                let ranked = RankedSources::rank(r.sources, &filters, &caps);
                pick_for_binge(&ranked, Some(&previous)).cloned()
            });
            match pick {
                Some(source) => {
                    let mut request = request;
                    request.preferred = Some(source);
                    c.open_sources(request);
                }
                None => {
                    c.stop_playback(false);
                    c.pop_screen();
                    c.show_picker(request);
                }
            }
        });
    }

    fn open_track_menu(&mut self, kind: TrackKind) {
        let Some(player) = self.video.player() else { return };
        let tracks: Vec<_> = player.tracks().into_iter().filter(|t| t.kind == kind).collect();
        let mut entries: Vec<(Option<i64>, String, bool)> = Vec::new();
        if kind == TrackKind::Subtitle {
            entries.push((None, "Off".into(), !tracks.iter().any(|t| t.selected)));
        }
        entries.extend(tracks.iter().map(|t| (Some(t.id), t.label(), t.selected)));
        if entries.is_empty() {
            self.toast("No other tracks in this file.");
            return;
        }
        let index = entries.iter().position(|(_, _, selected)| *selected).unwrap_or(0);
        if let Some(p) = self.player.as_mut() {
            p.menu = Some(TrackMenu { kind, entries, index });
        }
    }

    fn show_overlay(&mut self) {
        if let Some(p) = self.player.as_mut() {
            p.overlay_until = Instant::now() + Duration::from_secs(OVERLAY_SECS);
        }
    }

    /// The fast decoder path stalled: remember compatibility mode for this TV and
    /// continue on a fresh mpv instance (the stalled one can't be stopped).
    fn recover_with_compat_mode(&mut self) {
        let Some(p) = self.player.take() else { return };
        self.save_settings(|s| s.video_compat_mode = Some(true));
        match crate::video::VideoHost::create_player(true) {
            Ok(player) => {
                self.video.replace_player(player);
                let mut request = p.request.clone();
                request.resume_at = Some(p.position.max(p.request.resume_at.unwrap_or(0.0)));
                self.toast("Switched to compatibility video mode for this TV");
                self.start_playback(request, p.source, p.url);
            }
            Err(e) => self.toast(&format!("The video player couldn't restart: {e}")),
        }
    }

    /// Pause and save when the app leaves the screen.
    pub fn pause_for_background(&mut self) {
        if self.player.is_some() {
            if let Some(player) = self.video.player() {
                let _ = player.set_paused(true);
            }
            self.save_progress(false);
        }
    }

    pub fn player_key(&mut self, key: Key, repeat: bool) -> bool {
        let Some(player) = self.video.player() else {
            self.exit_player();
            return true;
        };
        let Some(p) = self.player.as_mut() else {
            self.pop_screen();
            return true;
        };
        let overlay_was_visible = Instant::now() < p.overlay_until || p.paused;

        // Remote media keys work regardless of focus.
        match key {
            Key::PlayPause => {
                let _ = player.toggle_pause();
            }
            Key::Play => {
                let _ = player.set_paused(false);
            }
            Key::Pause => {
                let _ = player.set_paused(true);
            }
            Key::FastForward | Key::Rewind => {
                let step = if key == Key::FastForward { 30.0 } else { -10.0 };
                let base = p.seek_target.unwrap_or(p.position);
                let max = if p.duration > 0.0 { p.duration - 1.0 } else { f64::MAX };
                p.seek_target = Some((base + step).clamp(0.0, max));
                p.seek_commit_at = Some(Instant::now() + Duration::from_millis(650));
                p.zone = 0;
            }
            Key::Stop => {
                self.exit_player();
                return true;
            }
            Key::Next => {
                if p.next.request.is_some() {
                    self.play_next();
                }
                return true;
            }
            Key::Captions => self.open_track_menu(TrackKind::Subtitle),
            Key::AudioTrack => self.open_track_menu(TrackKind::Audio),
            _ => {}
        }
        if matches!(
            key,
            Key::PlayPause | Key::Play | Key::Pause | Key::FastForward | Key::Rewind | Key::Captions | Key::AudioTrack
        ) {
            self.show_overlay();
            self.push_player();
            return true;
        }
        let Some(p) = self.player.as_mut() else { return true };

        // Track menu.
        if let Some(menu) = p.menu.as_mut() {
            match key {
                Key::Up => menu.index = menu.index.saturating_sub(1),
                Key::Down => menu.index = (menu.index + 1).min(menu.entries.len() - 1),
                Key::Ok => {
                    let (id, _, _) = menu.entries[menu.index].clone();
                    let _ = player.select_track(menu.kind, id);
                    p.menu = None;
                    self.refresh_subtitle_state();
                }
                Key::Back | Key::Left | Key::Right => p.menu = None,
                _ => {}
            }
            self.show_overlay();
            self.push_player();
            return true;
        }

        // Next-episode card.
        if p.next.focused {
            match key {
                Key::Ok => {
                    if p.next.request.is_some() {
                        self.play_next();
                    } else {
                        self.exit_player();
                    }
                    return true;
                }
                Key::Down | Key::Left => p.next.focused = false,
                Key::Back => {
                    p.next.focused = false;
                    p.next.dismissed = true;
                    p.next.countdown_from = None;
                }
                _ => {}
            }
            self.push_player();
            return true;
        }

        match key {
            Key::Back => {
                if p.error.is_none() && p.seek_target.is_some() {
                    p.seek_target = None;
                    p.seek_commit_at = None;
                } else if p.next.countdown_from.is_some() {
                    p.next.countdown_from = None;
                    p.next.dismissed = true;
                } else {
                    self.exit_player();
                    return true;
                }
            }
            Key::Ok if p.next.countdown_from.is_some() => {
                self.play_next();
                return true;
            }
            Key::Ok if !overlay_was_visible => {
                let _ = player.toggle_pause();
            }
            Key::Left | Key::Right if p.zone == 0 || !overlay_was_visible => {
                p.zone = 0;
                let step = match (key, repeat) {
                    (Key::Left, false) => -10.0,
                    (Key::Left, true) => -30.0,
                    (_, false) => 10.0,
                    (_, true) => 30.0,
                };
                let base = p.seek_target.unwrap_or(p.position);
                let max = if p.duration > 0.0 { p.duration - 1.0 } else { f64::MAX };
                p.seek_target = Some((base + step).clamp(0.0, max));
                p.seek_commit_at = Some(Instant::now() + Duration::from_millis(650));
            }
            Key::Ok if p.zone == 0 => {
                if let Some(target) = p.seek_target.take() {
                    p.seek_commit_at = None;
                    p.position = target;
                    let _ = player.seek_to(target);
                } else {
                    let _ = player.toggle_pause();
                }
            }
            Key::Down if p.zone == 0 && overlay_was_visible => p.zone = 1,
            Key::Up if p.zone == 0 && p.next.shown && !p.next.dismissed => p.next.focused = true,
            Key::Up if p.zone == 1 => p.zone = 0,
            Key::Left if p.zone == 1 => p.control = p.control.saturating_sub(1),
            Key::Right if p.zone == 1 => p.control = (p.control + 1).min(p.controls() - 1),
            Key::Ok if p.zone == 1 => match p.control {
                0 => {
                    let _ = player.toggle_pause();
                }
                1 => {
                    let _ = player.seek_by(-10.0);
                }
                2 => {
                    let _ = player.seek_by(30.0);
                }
                3 => self.open_track_menu(TrackKind::Audio),
                4 => self.open_track_menu(TrackKind::Subtitle),
                5 => {
                    p.night = !p.night;
                    let night = p.night;
                    let _ = player.set_night_mode(night);
                    self.toast(if night { "Night mode on: quieter loud scenes, clearer dialogue" } else { "Night mode off" });
                }
                6 => {
                    self.play_next();
                    return true;
                }
                _ => {}
            },
            _ => {}
        }
        self.show_overlay();
        self.push_player();
        true
    }
}
