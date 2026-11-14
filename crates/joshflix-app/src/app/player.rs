//! Playback: overlay, seeking, tracks, Audio Normalization, progress and next episode.

use std::time::{Duration, Instant};

use joshflix_core::VideoRef;
use joshflix_core::sources::{RankedSources, Source, pick_for_binge};
use joshflix_core::store::{LastSource, Progress};
use joshflix_player::{EndReason, PlayerEvent, TrackKind};
use slint::{Global, Model, ModelRc};

use super::detail::{PlayContext, PlayRequest};
use super::{Controller, Key, shared};
use crate::format;
use crate::platform::AudioNormalization;
use crate::{PlayerState, Screen, TrackItem};

const OVERLAY_SECS: u64 = 5;
/// How long the title and seek bar show when the first frame appears.
const FIRST_FRAME_OVERLAY: Duration = Duration::from_secs(4);
const SAVE_EVERY: Duration = Duration::from_secs(10);
/// The next-episode card shows for the last minute. Autoplay itself waits
/// for the end of the file, so an episode always plays to its last frame.
const NEXT_CARD_SECS: f64 = 60.0;

/// Give up waiting for a stream to open after this long.
const OPEN_TIMEOUT: Duration = Duration::from_secs(45);
/// Loaded and well buffered, yet no picture after this long: the decoder is stuck.
const DECODE_TIMEOUT: Duration = Duration::from_secs(40);
/// How long playback may run without picture on mpv's own surface before the
/// app switches to drawing video into the UI.
const NO_PICTURE_TIMEOUT: Duration = Duration::from_secs(8);
/// How long video must show on mpv's own surface before that counts as working.
const PICTURE_CONFIRM: Duration = Duration::from_secs(3);
/// How often the TV log gets a dropped-frames line during playback.
const DROPS_LOG_EVERY: Duration = Duration::from_secs(10);
/// A stream that drops after playing is reopened where it stopped, this many
/// times per title before the player gives up and says so.
const MAX_RECONNECTS: u32 = 4;

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
    /// Autoplay will start the next episode when this one ends; the card
    /// counts down the time left.
    autoplay_armed: bool,
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
    pub buffering: Option<i64>,
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
    pub error: Option<String>,
    subtitles: Vec<joshflix_core::subtitles::Subtitle>,
    subtitles_added: bool,
    /// Pick the subtitle track once the track list (incl. added subs) is known.
    subtitle_choice_pending: bool,
    subtitles_on: bool,
    /// When the file loaded, to detect a stalled zero-copy decoder.
    loaded_at: Option<Instant>,
    /// mpv reported PLAYBACK_RESTART (the first frame is up) for this file.
    pub started: bool,
    /// Seconds buffered ahead (tells a stalled decoder from a slow network).
    cache_ahead: f64,
    /// Next-episode lookup in flight.
    autoplay_pending: bool,
    /// The next episode being looked up after this one stopped: the start
    /// cover shows it meanwhile.
    pub pending_next: Option<PlayRequest>,
    /// mpv gave up on this stream after it had played: Back offers other sources.
    source_failed: bool,
    hwdec_fallback: bool,
    /// When playback was requested (to time out a stream that never opens).
    pub opened_at: Instant,
    /// The open timed out; cleared if the file loads after all.
    open_timed_out: bool,
    /// mpv's latest error message, shown with a failure.
    last_mpv_error: Option<String>,
    /// Last overlay visibility pushed to mpv (to lift subtitles above the controls).
    overlay_shown: Option<bool>,
    /// Audio Normalization is on.
    normalize: bool,
    /// Decoded frame width, and the width mpv's video output is set up for
    /// (0 when none). Equal and nonzero means the output shows this video:
    /// with force-window or vo=null the output can have a dummy size.
    video_width: i64,
    video_out_width: i64,
    /// Own-surface mode: since when playback has run without picture, or with it.
    no_picture_since: Option<Instant>,
    picture_since: Option<Instant>,
    /// Frames dropped so far by the video output and the decoder, and the
    /// totals at the last log line.
    drops: (i64, i64),
    drops_logged: (i64, i64),
    drops_logged_at: Instant,
    /// Times this title's stream was reopened after dropping mid-play.
    reconnects: u32,
    /// The stream dropped while the app was in the background: reopen it on return.
    pub reconnect_on_return: bool,
    /// The user picked a subtitle track from the menu.
    subtitle_user_choice: bool,
    /// Back hid the controls while paused.
    overlay_dismissed: bool,
    /// The subtitle track (or none) to pick again once this reopened stream lists it.
    restore_subtitle: Option<Option<i64>>,
    /// The subtitle track (or none) the user picked from the menu.
    user_subtitle: Option<Option<i64>>,
    /// A reopen is scheduled (later attempts wait for the network).
    reconnect_waiting: bool,
}

impl PlayerCtl {
    /// The stream is being reopened after it dropped mid-play.
    pub fn reconnecting(&self) -> bool {
        (self.reconnects > 0 && !self.started) || self.reconnect_waiting
    }

    fn controls(&self) -> usize {
        if self.next.request.is_some() { CONTROL_COUNT_WITH_NEXT } else { CONTROL_COUNT }
    }
}

impl Controller {
    pub fn start_playback(&mut self, request: PlayRequest, source: Source, url: String) {
        let Some(player) = self.video.player() else {
            let reason = self.video.unavailable_reason().unwrap_or_default();
            self.toast(&format!("The video player isn't available. {reason}"));
            self.push_start();
            return;
        };
        // Replace whatever is playing (next episode) without leaving the player.
        if self.player.is_some() {
            self.stop_playback(true);
        } else if self.screen != Screen::Player {
            self.back_stack.push(self.screen);
        }
        let wanted = self.settings.audio_normalization;
        let normalize = self.apply_audio_normalization(&player, wanted, false) && wanted;
        let _ = player.set_subtitle_scale(f64::from(self.settings.subtitle_scale));
        // The audio language picked last time (a multi-language release
        // shouldn't switch back to its default every episode).
        let _ = player.set_property("alang", self.settings.audio_language.as_deref().unwrap_or(""));
        crate::video::want_video(true);
        if let Err(e) = player.load(&url, request.resume_at) {
            crate::video::want_video(self.screen == Screen::Player);
            self.toast(&e.to_string());
            self.push_start();
            return;
        }
        if self.backgrounded {
            // The link resolved after the app left the screen: wait paused.
            let _ = player.set_paused(true);
        } else {
            self.platform.keep_screen_on(true);
        }
        tracing::info!(
            "play {} · {} · cached={} · stream host {}{}",
            request.video.video_id(),
            source.release.release_name,
            source.cached,
            url.split('/').nth(2).unwrap_or("?"),
            request.resume_at.filter(|s| *s > 1.0).map(|s| format!(" · resume at {s:.0} s")).unwrap_or_default(),
        );
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
            cache_ahead: 0.0,
            autoplay_pending: false,
            pending_next: None,
            source_failed: false,
            hwdec_fallback: false,
            opened_at: Instant::now(),
            open_timed_out: false,
            last_mpv_error: None,
            video_width: 0,
            video_out_width: 0,
            no_picture_since: None,
            picture_since: None,
            drops: (0, 0),
            drops_logged: (0, 0),
            drops_logged_at: Instant::now(),
            overlay_shown: None,
            normalize,
            reconnects: 0,
            reconnect_on_return: false,
            subtitle_user_choice: false,
            overlay_dismissed: false,
            restore_subtitle: None,
            user_subtitle: None,
            reconnect_waiting: false,
        });
        let (title, subtitle) = super::video_title(&request.context.name, &request.video, request.context.episode_title.as_deref());
        let state = PlayerState::get(&self.ui);
        state.set_title(shared(title));
        state.set_subtitle(shared(subtitle));
        state.set_error("".into());
        state.set_menu_open(false);
        state.set_next_visible(false);
        state.set_audio_normalization(normalize);
        state.set_buffering(false);
        self.set_screen(Screen::Player);
        self.push_player();

        // Some MP4s keep all their audio (or captions) far from the video, and
        // one connection can't keep up with the jumps between them. Read the
        // file's layout alongside mpv's own opening, and give those tracks
        // their own connections.
        if joshflix_core::mp4::is_mp4_url(&url) {
            let http = self.svc.http.clone();
            let probe_url = url.clone();
            let started = Instant::now();
            self.spawn(
                async move {
                    tokio::time::timeout(Duration::from_secs(20), joshflix_core::mp4::probe(&http, &probe_url)).await
                },
                move |c, result| {
                    let layout = match result {
                        Ok(Ok(Some(layout))) => layout,
                        Ok(Ok(None)) => return tracing::info!("mp4 layout: no usable moov"),
                        Ok(Err(e)) => return tracing::warn!("mp4 layout: {e}"),
                        Err(_) => return tracing::warn!("mp4 layout: no answer in 20 s"),
                    };
                    let separate = layout.separate();
                    let distance = |kind| {
                        layout
                            .tracks
                            .iter()
                            .filter(|t| t.kind == kind)
                            .filter_map(|t| layout.distance_from_video(t))
                            .max()
                            .map_or_else(|| "none".to_owned(), |d| format!("{} MiB", d >> 20))
                    };
                    tracing::info!(
                        "mp4 layout in {:.1} s: audio up to {} from the video, subtitles up to {}; own connections for {:?}",
                        started.elapsed().as_secs_f64(),
                        distance(joshflix_core::mp4::Mp4TrackKind::Audio),
                        distance(joshflix_core::mp4::Mp4TrackKind::Subtitle),
                        &separate,
                    );
                    if !separate.any() || c.player.as_ref().is_none_or(|p| p.url != url) {
                        return;
                    }
                    if let Some(player) = c.video.player() {
                        let ids = |ids: Vec<u32>| ids.into_iter().map(i64::from).collect();
                        let tracks = joshflix_player::SeparateTracks { audio: ids(separate.audio), subtitles: ids(separate.subtitles) };
                        let _ = player.separate_tracks(&url, tracks);
                    }
                },
            );
        }

        // Subtitle list from OpenSubtitles, added once the first frame shows:
        // mpv downloads each one, and the start shouldn't wait behind them.
        let langs = self.settings.subtitle_languages.clone();
        let subs = self.svc.subtitles.clone();
        let video = request.video.clone();
        self.spawn(async move { subs.find(&video, &langs, 3).await.unwrap_or_default() }, move |c, found| {
            if let Some(p) = c.player.as_mut() {
                p.subtitles = found;
                if p.started {
                    c.add_subtitles();
                }
            }
        });
    }

    /// Work out the next episode (and the card's wording) for a series.
    fn next_for(&self, request: &PlayRequest) -> NextCtl {
        let empty = NextCtl { request: None, header: String::new(), title: String::new(), autoplay_armed: false, focused: false, dismissed: false, shown: false };
        let (Some(detail), Some(season), Some(episode)) = (request.context.detail.as_deref(), request.video.season, request.video.episode) else {
            return empty;
        };
        let Some(next) = detail.next_episode(season, episode) else {
            return NextCtl { header: "FINAL EPISODE".into(), title: format!("That's the end of {}", detail.preview.name), ..empty };
        };
        if !next.is_released(&format::today()) {
            // A show still airing: nothing to autoplay yet.
            let label = VideoRef::episode(&request.video.meta_id, next.season, next.episode).episode_label().unwrap_or_default();
            let when = next.released.as_deref().and_then(format::air_date).map(|d| format!(" airs {d}")).unwrap_or_else(|| " hasn't aired yet".into());
            return NextCtl { header: "LATEST EPISODE".into(), title: format!("{label}{when}"), ..empty };
        }
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

    pub fn on_player_event(&mut self, instance: u64, event: PlayerEvent) {
        // Events from an abandoned (replaced) mpv instance must not touch this session.
        if instance != self.video.current_id() {
            return;
        }
        let Some(p) = self.player.as_mut() else { return };
        match event {
            // Already logged on mpv's thread; only errors are forwarded here.
            PlayerEvent::Log { text, .. } => {
                p.last_mpv_error = Some(text);
                return;
            }
            PlayerEvent::FileLoaded => {
                tracing::info!("file loaded after {:.1} s", p.opened_at.elapsed().as_secs_f64());
                if p.open_timed_out {
                    p.open_timed_out = false;
                    p.error = None;
                }
                p.video_shown = true;
                p.subtitle_choice_pending = !p.subtitle_user_choice;
                p.loaded_at = Some(Instant::now());
                self.set_screen(Screen::Player);
            }
            PlayerEvent::Started => {
                let first = !p.started;
                p.buffering = None;
                p.started = true;
                if first {
                    let total = self.sources.play_started.take().map(|t| format!(", {:.1} s after Play", t.elapsed().as_secs_f64()));
                    tracing::info!("first frame after {:.1} s{}", p.opened_at.elapsed().as_secs_f64(), total.unwrap_or_default());
                    // The cover dissolves into the video: show where it is for a moment.
                    p.overlay_until = Instant::now() + FIRST_FRAME_OVERLAY;
                    p.open_timed_out = false;
                    p.error = None;
                    self.add_subtitles();
                }
                // Compatibility mode proved itself: remember it for this TV.
                if self.pending_compat_save {
                    self.pending_compat_save = false;
                    self.save_settings(|s| s.video_compat_mode = Some(true));
                }
            }
            // High-frequency updates: just record them; the 250 ms tick redraws.
            PlayerEvent::Position(pos) => {
                p.position = pos;
                return;
            }
            PlayerEvent::Duration(d) => {
                p.duration = d;
                return;
            }
            PlayerEvent::CacheAhead(secs) => {
                p.cache_ahead = secs;
                return;
            }
            PlayerEvent::DroppedByOutput(n) => {
                p.drops.0 = n;
                return;
            }
            PlayerEvent::DroppedByDecoder(n) => {
                p.drops.1 = n;
                return;
            }
            PlayerEvent::VideoOutputWidth(width) => {
                p.video_out_width = width;
                return;
            }
            PlayerEvent::DecodedWidth(width) => {
                p.video_width = width;
                return;
            }
            PlayerEvent::Paused(paused) => {
                p.paused = paused;
                if paused {
                    p.overlay_until = Instant::now() + Duration::from_secs(3600);
                    self.save_progress(false);
                } else {
                    let p = self.player.as_mut().expect("player");
                    p.overlay_until = Instant::now() + Duration::from_secs(OVERLAY_SECS);
                    p.overlay_dismissed = false;
                }
            }
            PlayerEvent::Buffering(state) => p.buffering = state,
            PlayerEvent::TracksChanged => {
                // After a reconnect: the subtitle the user had picked.
                if let Some(wanted) = p.restore_subtitle
                    && let Some(player) = self.video.player()
                {
                    let tracks = player.tracks();
                    if wanted.is_none_or(|id| tracks.iter().any(|t| t.kind == TrackKind::Subtitle && t.id == id)) {
                        let _ = player.select_track(TrackKind::Subtitle, wanted);
                        if let Some(p) = self.player.as_mut() {
                            p.restore_subtitle = None;
                        }
                    }
                }
                self.choose_subtitle();
                self.refresh_subtitle_state();
            }
            PlayerEvent::FrameRate(fps) => {
                if self.settings.frame_rate_matching && fps > 1.0 {
                    self.platform.set_frame_rate(fps);
                }
            }
            PlayerEvent::Ended(EndReason::Eof) => {
                // A dropped connection also ends the file; only call it finished near the end.
                let near_end = p.duration > 0.0
                    && (p.position >= p.duration * joshflix_core::store::WATCHED_FRACTION || p.duration - p.position < 90.0);
                if !near_end {
                    // Only a stream that played counts as dropped (a link that
                    // ends at once is broken, and reopening it won't help).
                    if (p.started || p.reconnects > 0) && p.reconnects < MAX_RECONNECTS {
                        self.reconnect("the stream ended early");
                        return;
                    }
                    p.error = Some("The connection keeps dropping. Press Back and play again to resume, or try another source.".into());
                    p.buffering = None;
                    p.source_failed = true;
                    self.save_progress(false);
                } else if !p.finished {
                    p.finished = true;
                    self.on_finished();
                    return;
                } else {
                    return;
                }
            }
            PlayerEvent::Ended(EndReason::Error(message)) => {
                tracing::warn!("playback failed: {message} (last mpv error: {:?})", p.last_mpv_error);
                // A drop in the credits finishes the episode (autoplay goes on).
                let near_end = p.duration > 0.0
                    && (p.position >= p.duration * joshflix_core::store::WATCHED_FRACTION || p.duration - p.position < 90.0);
                if p.started && near_end {
                    if !p.finished {
                        p.finished = true;
                        self.on_finished();
                    }
                    return;
                }
                // It had played, so the link works: most likely the connection dropped.
                if (p.started || p.reconnects > 0) && p.reconnects < MAX_RECONNECTS {
                    self.reconnect("playback failed");
                    return;
                }
                p.source_failed = true;
                let detail = p.last_mpv_error.as_deref().map(|e| format!(" ({e})")).unwrap_or_default();
                p.error = Some(format!("Playback failed: {message}{detail}. Press Back and try another source."));
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
        // Re-evaluate the choice once these arrive (they can land after the
        // file loaded), unless the user already picked a track.
        p.subtitle_choice_pending = !p.subtitle_user_choice;
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
        self.choose_subtitle_now(false);
    }

    fn choose_subtitle_now(&mut self, force: bool) {
        let Some(p) = self.player.as_mut() else { return };
        if !p.subtitle_choice_pending {
            return;
        }
        let Some(player) = self.video.player() else { return };
        let tracks = player.tracks();
        if tracks.is_empty() {
            return;
        }
        let expected = if p.subtitles_added && !force { p.subtitles.len() } else { 0 };
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
        // Until the first frame, the start cover shows progress and errors.
        let live = p.started && p.pending_next.is_none();
        let overlay = live
            && ((!p.overlay_dismissed && (Instant::now() < p.overlay_until || p.paused))
                || p.menu.is_some()
                || p.seek_target.is_some()
                || p.error.is_some());
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
        let buffering = live && p.buffering.is_some() && p.error.is_none();
        state.set_buffering(buffering);
        state.set_buffering_text(shared(match p.buffering {
            Some(pct) if pct > 0 => format!("Buffering {pct}%"),
            _ => "Buffering…".to_owned(),
        }));
        // The clock and seek bar only change what's drawn while the controls
        // show: skipping them otherwise lets a still UI render no frames at
        // all during playback (each one is a translucent 1080p layer for the
        // compositor to blend over the video).
        if overlay {
            let progress = if p.duration > 0.0 { (p.position / p.duration) as f32 } else { 0.0 };
            state.set_progress(progress);
            state.set_position_text(shared(format::clock(p.position)));
            state.set_duration_text(shared(format::clock(p.duration)));
            state.set_remaining_text(shared(if p.duration > 0.0 { format!("-{}", format::clock(p.duration - p.position)) } else { String::new() }));
        }
        if let Some(target) = p.seek_target {
            state.set_seeking(true);
            state.set_seek_progress(if p.duration > 0.0 { (target / p.duration) as f32 } else { 0.0 });
            state.set_seek_text(shared(format::clock(target)));
        } else {
            state.set_seeking(false);
        }
        state.set_zone(p.zone);
        state.set_control_index(p.control as i32);
        state.set_audio_normalization(p.normalize);
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
                // One model for the menu's life: a new one rebuilds every row.
                let model = &self.track_menu_model;
                if model.iter().ne(options.iter().cloned()) {
                    model.set_vec(options);
                }
                state.set_menu_options(ModelRc::from(model.clone()));
                state.set_menu_index(menu.index as i32);
            }
            None => state.set_menu_open(false),
        }
        state.set_next_visible(p.next.shown && !p.next.dismissed);
        state.set_next_header(shared(&p.next.header));
        state.set_next_title(shared(&p.next.title));
        state.set_next_focused(p.next.focused);
        state.set_next_countdown(if p.next.autoplay_armed { (p.duration - p.position).max(0.0).ceil() as i32 } else { -1 });
        state.set_error(shared(if live { p.error.as_deref().unwrap_or_default() } else { "" }));
        self.push_start();
    }

    pub(super) fn player_tick(&mut self) {
        let Some(p) = self.player.as_mut() else { return };
        if p.subtitle_choice_pending && p.loaded_at.is_some_and(|t| t.elapsed() > Duration::from_secs(6)) {
            // Some OpenSubtitles downloads never arrive; apply the preference anyway.
            self.choose_subtitle_now(true);
        }
        let Some(p) = self.player.as_mut() else { return };
        // Some TV/emulator drivers accept MediaCodec's zero-copy output but never
        // deliver frames. If 8 s after loading nothing has played although 5 s of
        // media is buffered (so it isn't the network), continue on a new mpv
        // instance in copy-back mode, which works everywhere.
        if cfg!(target_os = "android")
            && !self.video.is_compat()
            && !p.hwdec_fallback
            && !p.paused
            && !p.started
            && p.cache_ahead >= 5.0
            && p.loaded_at.is_some_and(|t| t.elapsed() > Duration::from_secs(8))
        {
            p.hwdec_fallback = true;
            tracing::warn!("no frames from zero-copy MediaCodec; switching to compatibility mode");
            self.recover_with_compat_mode();
            return;
        }
        // The output shows this file's frames: with force-window or vo=null it
        // can be set up for a dummy size without any video.
        let output_shows_video = p.video_out_width > 0 && p.video_out_width == p.video_width;
        // While the next episode is looked up, the old file is already stopped.
        let playing = p.started && !p.paused && p.buffering.is_none() && p.error.is_none() && p.pending_next.is_none();
        // Own-surface mode: playing with a video track but no picture means mpv
        // couldn't show video on its surface. Fall back to drawing it into the
        // UI. No picture is expected while the surface itself is missing.
        if crate::video::surface_mode() && crate::video::own_surface_ready() {
            let player = self.video.player();
            let has_video = player.as_ref().is_some_and(|pl| pl.tracks().iter().any(|t| t.kind == TrackKind::Video));
            let picture = output_shows_video && player.as_ref().is_some_and(crate::video::shows_on_own_surface);
            if playing && has_video && !picture {
                p.picture_since = None;
                let since = *p.no_picture_since.get_or_insert_with(Instant::now);
                if since.elapsed() > NO_PICTURE_TIMEOUT {
                    tracing::warn!(
                        "no picture {NO_PICTURE_TIMEOUT:?} into playback on mpv's own surface (output width {}, video width {})",
                        p.video_out_width,
                        p.video_width
                    );
                    self.recover_with_embedded_video();
                    return;
                }
            } else {
                p.no_picture_since = None;
            }
            if playing && picture {
                let since = *p.picture_since.get_or_insert_with(Instant::now);
                if since.elapsed() > PICTURE_CONFIRM {
                    crate::video::confirm_own_surface();
                }
            }
        } else {
            p.no_picture_since = None;
            // After a fallback from the own surface: once video drawn into the
            // UI shows, the own surface was the problem (not the file).
            if crate::video::fallback_pending() && playing && output_shows_video {
                let since = *p.picture_since.get_or_insert_with(Instant::now);
                if since.elapsed() > PICTURE_CONFIRM {
                    crate::video::confirm_fallback();
                }
            } else {
                p.picture_since = None;
            }
        }
        // Minutes of steady playback since the last reopen: a new budget.
        if p.reconnects > 0 && p.started && p.loaded_at.is_some_and(|t| t.elapsed() > Duration::from_secs(180)) {
            p.reconnects = 0;
        }
        if p.drops_logged_at.elapsed() >= DROPS_LOG_EVERY {
            if p.started && !p.paused && p.pending_next.is_none() && p.drops != p.drops_logged {
                tracing::info!(
                    "playback drops: {} frames by the video output, {} by the decoder in {:.0} s",
                    p.drops.0 - p.drops_logged.0,
                    p.drops.1 - p.drops_logged.1,
                    p.drops_logged_at.elapsed().as_secs_f64(),
                );
            }
            p.drops_logged = p.drops;
            p.drops_logged_at = Instant::now();
        }
        // Never spin forever: say what went wrong instead.
        // (Not while a reopen waits for the network: its timer decides.)
        if p.error.is_none() && !p.video_shown && !p.reconnect_waiting && p.opened_at.elapsed() > OPEN_TIMEOUT {
            tracing::warn!("stream didn't open within {OPEN_TIMEOUT:?} (last mpv error: {:?})", p.last_mpv_error);
            let detail = p.last_mpv_error.as_deref().map(|e| format!(" ({e})")).unwrap_or_default();
            p.error = Some(format!("The stream didn't open{detail}. Press Back and try another source."));
            p.open_timed_out = true;
            p.buffering = None;
        }
        if p.error.is_none()
            && !p.started
            && !p.reconnect_waiting
            && !p.paused
            && p.cache_ahead >= 10.0
            && p.loaded_at.is_some_and(|t| t.elapsed() > DECODE_TIMEOUT)
        {
            tracing::warn!("no picture {DECODE_TIMEOUT:?} after loading with {:.0} s buffered", p.cache_ahead);
            p.error = Some(
                "This TV couldn't decode this video. Press Back and try another source (a 1080p one is safest).".into(),
            );
            p.buffering = None;
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
        if near_end && !p.next.dismissed && !p.next.shown && !p.next.header.is_empty() && p.pending_next.is_none() {
            p.next.shown = true;
            if p.next.request.is_some() && self.settings.autoplay_next {
                p.next.autoplay_armed = true;
            }
            // Look up the next episode's sources now, so it starts at once.
            if let Some(next) = p.next.request.as_ref().map(|r| r.video.clone()) {
                self.prefetch_streams(&next);
                self.next_sources_ready(&next.video_id());
            }
        }
        let p = self.player.as_mut().expect("player");
        if !near_end && p.next.shown && remaining > NEXT_CARD_SECS {
            // Seeked back out of the credits.
            p.next.shown = false;
            p.next.autoplay_armed = false;
            p.next.focused = false;
        }
        // The next episode starts from `on_finished` at the end of the file.
        // A finished episode stays watched while the next one loads.
        let save_due = p.video_shown && p.last_save.elapsed() >= SAVE_EVERY && !p.paused && !p.finished && p.pending_next.is_none();
        // For the house lights (through the relay); buffering doesn't count as a pause.
        let (name, episode) =
            super::video_title(&p.request.context.name, &p.request.video, p.request.context.episode_title.as_deref());
        let title = if episode.is_empty() { name } else { format!("{name} · {episode}") };
        crate::screen_colors::set_now_playing(
            p.started && !p.paused && p.error.is_none() && p.pending_next.is_none(),
            Some(&title),
        );
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
            hidden: false,
            marked_from: None,
            queued: None,
            queued_prev_at: None,
            marked_prev_at: None,
        };
        let watched = progress.is_watched();
        // Finished: the show moves on to the next episode in Continue Watching.
        let next = p.next.request.as_ref().filter(|_| watched).map(|next| joshflix_core::store::LaterEpisode {
            video: next.video.clone(),
            title: next.context.episode_title.clone(),
            thumbnail: next.context.thumbnail.clone(),
        });
        // Database writes fsync: they go to the ordered writer thread, never
        // the thread that renders video. The page the player returns to read
        // the store before this landed, so it refreshes when it does.
        self.store_write(
            move |store| {
                if let Err(e) = store.save_progress_and_queue(progress, next) {
                    tracing::warn!("save progress: {e}");
                }
            },
            |c, ()| c.progress_saved(),
        );
    }

    /// A progress write landed: refresh what shows it, unless still playing.
    fn progress_saved(&mut self) {
        match self.screen {
            Screen::Detail => self.refresh_detail_after_playback(),
            Screen::Browse | Screen::Mylist if !self.rail_focus => self.refresh_local_rows(),
            _ => {}
        }
    }

    fn on_finished(&mut self) {
        if self.player.as_ref().is_some_and(|p| p.autoplay_pending) {
            return; // the next episode is already being looked up
        }
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
        self.autoplay_generation += 1; // drop any in-flight next-episode lookup
        // A start cancelled before its first frame mustn't time the next one.
        if self.player.as_ref().is_some_and(|p| !p.started) {
            self.sources.play_started = None;
        }
        // A finished episode was already saved as watched; don't overwrite it.
        if save && self.player.as_ref().is_some_and(|p| !p.finished) {
            self.save_progress(false);
        }
        if let Some(player) = self.video.player() {
            let _ = player.stop();
        }
        crate::screen_colors::set_now_playing(false, None);
        if self.player.take().is_some() {
            self.platform.keep_screen_on(false);
            if self.settings.frame_rate_matching {
                self.platform.set_frame_rate(0.0);
            }
        }
        self.push_start();
    }

    fn exit_player(&mut self) {
        self.stop_playback(true);
        self.pop_screen();
    }

    fn play_next(&mut self) {
        let Some(p) = self.player.as_mut() else { return };
        if p.autoplay_pending {
            return;
        }
        let Some(request) = p.next.request.clone() else { return };
        p.next.autoplay_armed = false;
        p.autoplay_pending = true;
        let previous = p.source.clone();
        self.save_progress(true);
        // Stop the credits now; the start cover shows the next episode while
        // its stream is found.
        if let Some(player) = self.video.player() {
            let _ = player.stop();
        }
        if let Some(p) = self.player.as_mut() {
            p.finished = true;
            p.pending_next = Some(request.clone());
            p.next.shown = false;
            p.next.focused = false;
            p.menu = None;
            p.seek_target = None;
        }
        self.push_player();
        self.autoplay(request, previous);
    }

    /// Sources for `video_id` arrived: if it's the next episode, pick its
    /// release and resolve the link ahead, so autoplay starts at once.
    pub(super) fn next_sources_ready(&mut self, video_id: &str) {
        let Some(p) = self.player.as_ref() else { return };
        let Some(next) = p.next.request.as_ref().filter(|n| n.video.video_id() == video_id) else { return };
        let Some(found) = self.fresh_streams(video_id) else { return };
        let ranked = RankedSources::rank(found.sources, &self.settings.filters, &self.caps);
        if let Some(pick) = pick_for_binge(&ranked, Some(&p.source)).cloned() {
            let video = next.video.clone();
            self.resolve_ahead(&video, &pick);
        }
    }

    /// Next episode: pick the same release family as before and play it, or
    /// fall back to the picker.
    fn autoplay(&mut self, request: PlayRequest, previous: Source) {
        let Some(comet) = self.comet() else { return };
        let video = request.video.clone();
        let (filters, caps) = (self.settings.filters, self.caps);
        if let Some(p) = self.player.as_mut() {
            p.error = None;
        }
        self.autoplay_generation += 1;
        let generation = self.autoplay_generation;
        // Usually fetched while the next-episode card showed.
        let ahead = self.fresh_streams(&video.video_id());
        self.spawn(
            async move {
                match ahead {
                    Some(found) => Ok(found),
                    None => comet.streams(&video).await,
                }
            },
            move |c, result| {
            // The user left the player (or another lookup started) meanwhile.
            if c.autoplay_generation != generation || c.player.is_none() || c.screen != Screen::Player {
                return;
            }
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
        },
        );
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
            p.overlay_dismissed = false;
            p.overlay_until = Instant::now() + Duration::from_secs(OVERLAY_SECS);
        }
    }

    /// Switch Audio Normalization with the platform's effect, or mpv's filter
    /// where the platform has none. Returns false (after a toast when `tell`)
    /// if this device can't normalize audio.
    fn apply_audio_normalization(&mut self, player: &joshflix_player::Player, on: bool, tell: bool) -> bool {
        match self.platform.set_audio_normalization(on) {
            AudioNormalization::Platform => true,
            AudioNormalization::Player => {
                if let Err(e) = player.set_audio_normalization(on) {
                    tracing::warn!("audio normalization: {e}");
                }
                true
            }
            AudioNormalization::Unavailable => {
                if tell && on {
                    self.toast("Audio Normalization isn't available on this TV");
                }
                false
            }
        }
    }

    /// The fast decoder path stalled: remember compatibility mode for this TV and
    /// continue on a fresh mpv instance (the stalled one can't be stopped).
    fn recover_with_compat_mode(&mut self) {
        let Some(p) = self.player.take() else { return };
        // Saved once compatibility mode actually shows frames (see Started).
        self.pending_compat_save = true;
        match crate::video::VideoHost::create_player(true, self.platform.audio_session()) {
            Ok(player) => {
                self.video.replace_player(player, true);
                let mut request = p.request.clone();
                request.resume_at = Some(p.position.max(p.request.resume_at.unwrap_or(0.0)));
                self.toast("Switched to compatibility video mode for this TV");
                self.start_playback(request, p.source, p.url);
            }
            Err(e) => self.toast(&format!("The video player couldn't restart: {e}")),
        }
    }

    /// Video never appeared on mpv's own surface: draw it into the UI from now
    /// on, and continue on a fresh mpv instance that does.
    fn recover_with_embedded_video(&mut self) {
        crate::video::give_up_own_surface();
        let Some(p) = self.player.take() else { return };
        let compat = self.video.is_compat();
        match crate::video::VideoHost::create_player(compat, self.platform.audio_session()) {
            Ok(player) => {
                self.video.replace_player(player, compat);
                let mut request = p.request.clone();
                request.resume_at = Some(p.position.max(p.request.resume_at.unwrap_or(0.0)));
                self.toast("Switched to the classic video mode for this TV");
                self.start_playback(request, p.source, p.url);
            }
            Err(e) => self.toast(&format!("The video player couldn't restart: {e}")),
        }
    }

    /// The stream dropped after playing: open the same link again where it
    /// stopped. The start cover says so meanwhile. Left in the background,
    /// it waits until the app is back.
    pub(super) fn reconnect(&mut self, why: &str) {
        let backgrounded = self.backgrounded;
        let Some(p) = self.player.as_mut() else { return };
        if backgrounded {
            tracing::warn!("{why} in the background; reopening on return");
            p.reconnect_on_return = true;
            p.buffering = None;
            return;
        }
        let attempt = p.reconnects + 1;
        // The first reopen is immediate; later ones wait 3, 6, 9 s for the network to come back.
        let delay = Duration::from_secs(3 * u64::from(attempt - 1));
        if !delay.is_zero() && !p.reconnect_waiting {
            p.reconnect_waiting = true;
            p.buffering = None;
            tracing::warn!("{why}; reopening in {} s", delay.as_secs());
            let (url, why) = (p.url.clone(), why.to_owned());
            slint::Timer::single_shot(delay, move || {
                super::with_controller(|c| {
                    if c.player.as_ref().is_some_and(|p| p.url == url && p.reconnect_waiting) {
                        c.reconnect(&why);
                    }
                });
            });
            self.push_player();
            return;
        }
        let mut request = p.request.clone();
        let at = p.seek_target.unwrap_or(p.position);
        request.resume_at = Some(at).filter(|s| *s > 1.0).or(request.resume_at);
        let (source, url, next) = (p.source.clone(), p.url.clone(), (p.next.dismissed, p.next.shown));
        // Read from the session, not mpv: its track list is gone once the file ended.
        let user_subtitle = p.user_subtitle;
        tracing::warn!("{why}; reopening the stream at {at:.0} s (attempt {attempt} of {MAX_RECONNECTS})");
        self.save_progress(false);
        // Keep the session's screen-on and frame-rate state: no stop_playback,
        // which would switch the display mode back and forth.
        self.player = None;
        self.start_playback(request, source, url);
        if let Some(p) = self.player.as_mut() {
            p.reconnects = attempt;
            (p.next.dismissed, p.next.shown) = next;
            if let Some(subtitle) = user_subtitle {
                p.subtitle_user_choice = true;
                p.user_subtitle = Some(subtitle);
                p.restore_subtitle = Some(subtitle);
            }
        }
        self.push_player();
    }

    /// Pause and save when the app leaves the screen.
    pub fn pause_for_background(&mut self) {
        if self.player.as_ref().is_some_and(|p| !p.finished && p.pending_next.is_none()) {
            if let Some(player) = self.video.player() {
                let _ = player.set_paused(true);
            }
            self.save_progress(false);
        }
    }

    /// Keys before the first frame (the start cover shows): Back cancels, or
    /// after an error opens the picker; OK tries the source again.
    fn starting_key(&mut self, key: Key) -> bool {
        let Some(p) = self.player.as_ref() else { return true };
        let failed = p.error.is_some() && p.pending_next.is_none();
        match key {
            Key::Back | Key::Stop => {
                let mut request = p.request.clone();
                // Nothing has played (the saved position stays), unless this
                // is a reopen after a drop: then keep where it got to.
                let played = p.started;
                self.stop_playback(played);
                self.pop_screen();
                if failed {
                    request.preferred = None;
                    self.show_picker(request);
                }
            }
            Key::Ok if failed => {
                let (request, source) = (p.request.clone(), p.source.clone());
                self.stop_playback(false);
                // A fresh link: the old one may be what failed.
                self.resolve(source, request, true);
            }
            _ => {}
        }
        true
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
        if !p.started || p.pending_next.is_some() || p.reconnecting() {
            return self.starting_key(key);
        }
        let overlay_was_visible = (Instant::now() < p.overlay_until || p.paused) && !p.overlay_dismissed;

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
                let max = if p.duration > 0.0 { (p.duration - 1.0).max(0.0) } else { f64::MAX };
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
                    let kind = menu.kind;
                    let _ = player.select_track(kind, id);
                    if kind == TrackKind::Subtitle {
                        // Later OpenSubtitles results mustn't undo this choice.
                        p.subtitle_choice_pending = false;
                        p.subtitle_user_choice = true;
                        p.user_subtitle = Some(id);
                    }
                    p.menu = None;
                    self.refresh_subtitle_state();
                    // Remember the choice for the next episode.
                    let lang = id.and_then(|id| player.tracks().into_iter().find(|t| t.id == id && t.kind == kind)).and_then(|t| t.lang);
                    match kind {
                        TrackKind::Audio if lang.is_some() && lang != self.settings.audio_language => {
                            self.save_settings(|s| s.audio_language = lang);
                        }
                        TrackKind::Subtitle if id.is_some() != self.settings.subtitles_enabled => {
                            self.save_settings(|s| s.subtitles_enabled = id.is_some());
                        }
                        _ => {}
                    }
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
                    p.next.autoplay_armed = false;
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
                } else if p.next.autoplay_armed {
                    p.next.autoplay_armed = false;
                    p.next.dismissed = true;
                } else if overlay_was_visible && p.error.is_none() {
                    // First Back hides the controls (a pause stays paused); the next one leaves.
                    p.overlay_until = Instant::now();
                    p.overlay_dismissed = p.paused;
                    p.zone = 0;
                    self.push_player();
                    return true;
                } else if p.source_failed {
                    // Resume would pick this source again: offer the others.
                    let mut request = p.request.clone();
                    request.preferred = None;
                    request.resume_at = Some(p.position).filter(|s| *s > 1.0);
                    self.exit_player();
                    self.show_picker(request);
                    return true;
                } else {
                    self.exit_player();
                    return true;
                }
            }
            Key::Ok if p.next.autoplay_armed && !overlay_was_visible && p.seek_target.is_none() => {
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
                let max = if p.duration > 0.0 { (p.duration - 1.0).max(0.0) } else { f64::MAX };
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
                    p.normalize = !p.normalize;
                    let normalize = p.normalize;
                    if self.apply_audio_normalization(&player, normalize, true) {
                        self.toast(if normalize {
                            "Audio Normalization on: quieter loud scenes, clearer dialogue"
                        } else {
                            "Audio Normalization off"
                        });
                    } else if let Some(p) = self.player.as_mut() {
                        p.normalize = false;
                    }
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
