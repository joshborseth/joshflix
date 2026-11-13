//! The Stremio-style source picker, the "getting your stream" step, and the
//! work done ahead (sources, stream links) so both start without waiting.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use joshflix_core::comet::{Comet, CometResult};
use joshflix_core::realdebrid::{RdErrorKind, RealDebrid, ResolveProgress, TORRENTS_URL};
use joshflix_core::release::Hidden;
use joshflix_core::sources::{RankedSources, Source};
use joshflix_core::{CoreError, VideoRef};
use slint::{Global, ModelRc, VecModel};
use tokio::sync::watch;

use super::detail::PlayRequest;
use super::{Controller, Key, Section, shared, with_controller};
use crate::format;
use crate::{Screen, SourceItem, SourcesState};

/// Comet's answer for a video stays good this long (cached flags can change).
const STREAMS_FRESH: Duration = Duration::from_secs(10 * 60);
/// A stream link resolved ahead stays usable this long.
const RESOLVED_FRESH: Duration = Duration::from_secs(30 * 60);
/// Results kept for videos fetched ahead.
const STREAMS_KEPT: usize = 24;

#[derive(Default)]
pub struct SourcesCtl {
    pub open: bool,
    pub request: Option<PlayRequest>,
    ranked: Option<RankedSources>,
    shown: Vec<(Source, Option<Hidden>)>,
    show_all: bool,
    index: usize,
    generation: u64,
    notices: Vec<String>,
    key_problem: bool,
    /// The open picker waits for this video's sources.
    waiting: Option<String>,
    /// Searching failed or found nothing: OK searches again.
    can_retry: bool,
    pub resolving: Option<ResolveCtl>,
    /// Comet results by video id, fetched ahead so the picker opens filled.
    streams: HashMap<String, (Instant, CometResult)>,
    streams_inflight: HashSet<String>,
    /// Direct stream links resolved ahead (Resume, next episode), by [`resolved_key`].
    resolved: HashMap<String, (Instant, String)>,
    resolving_ahead: HashSet<String>,
    /// When the current Play started, for the TV log's start timings.
    pub play_started: Option<Instant>,
}

pub struct ResolveCtl {
    cancel: watch::Sender<bool>,
    started: Instant,
    downloading_since: Option<Instant>,
    pub request: PlayRequest,
    source: Source,
    pub error: Option<String>,
    /// Came from Resume/next episode without the picker.
    direct: bool,
    generation: u64,
    pub status: String,
    /// Real-Debrid's download progress (0..1), or < 0 when unknown.
    pub progress: f32,
    pub hint: String,
}

/// One stream of one video.
fn resolved_key(video: &VideoRef, source: &Source) -> String {
    format!(
        "{}|{}|{}|{}",
        video.video_id(),
        source.info_hash,
        source.file_idx.map(|i| i.to_string()).unwrap_or_default(),
        source.filename.as_deref().unwrap_or_default()
    )
}

impl Controller {
    pub(super) fn comet(&self) -> Option<Comet> {
        let key = self.rd_key()?;
        // Development can point at a mock Comet.
        if let Ok(base) = std::env::var("JOSHFLIX_COMET_URL") {
            return Some(Comet::with_base(self.svc.http.clone(), base, &key));
        }
        Some(Comet::new(self.svc.http.clone(), &key))
    }

    fn real_debrid(&self) -> Option<RealDebrid> {
        self.rd_key().map(|key| RealDebrid::new(self.svc.http.clone(), key))
    }

    /// Entry point for every Play: skip the picker when a source is preferred.
    pub fn open_sources(&mut self, request: PlayRequest) {
        if self.rd_key().is_none() {
            self.toast("Add your Real-Debrid key in Settings first.");
            self.go_section(Section::Settings);
            return;
        }
        if let Some(source) = request.preferred.clone() {
            self.resolve(source, request, true);
            return;
        }
        self.show_picker(request);
    }

    // ---- work done ahead ----

    /// Comet's sources for `video` if fetched recently.
    pub(super) fn fresh_streams(&self, video_id: &str) -> Option<CometResult> {
        self.sources
            .streams
            .get(video_id)
            .filter(|(at, _)| at.elapsed() < STREAMS_FRESH)
            .map(|(_, result)| result.clone())
    }

    /// Fetch Comet's sources for `video` in the background (once), so the
    /// picker or the next episode doesn't wait for them.
    pub fn prefetch_streams(&mut self, video: &VideoRef) {
        let id = video.video_id();
        if self.fresh_streams(&id).is_some() || self.sources.streams_inflight.contains(&id) {
            return;
        }
        let Some(comet) = self.comet() else { return };
        self.sources.streams_inflight.insert(id.clone());
        let video = video.clone();
        let started = Instant::now();
        self.spawn(async move { comet.streams(&video).await }, move |c, result| {
            c.sources.streams_inflight.remove(&id);
            match &result {
                Ok(found) => {
                    tracing::info!("sources for {id}: {} in {:.1} s", found.sources.len(), started.elapsed().as_secs_f64());
                    // Empty answers aren't kept: "nothing yet" can change any minute.
                    if !found.sources.is_empty() {
                        if c.sources.streams.len() >= STREAMS_KEPT
                            && let Some(oldest) = c.sources.streams.iter().min_by_key(|(_, (at, _))| *at).map(|(k, _)| k.clone())
                        {
                            c.sources.streams.remove(&oldest);
                        }
                        c.sources.streams.insert(id.clone(), (Instant::now(), found.clone()));
                    }
                }
                Err(e) => tracing::warn!("sources for {id}: {e}"),
            }
            if c.sources.open && c.sources.waiting.as_deref() == Some(id.as_str()) {
                c.sources.waiting = None;
                c.sources_loaded(result);
            }
            c.next_sources_ready(&id);
        });
    }

    /// Resolve a cached source's stream link in the background, so pressing
    /// Play starts the video right away. Uncached sources are left alone:
    /// resolving those starts a Real-Debrid download.
    pub fn resolve_ahead(&mut self, video: &VideoRef, source: &Source) {
        if !source.cached {
            return;
        }
        let Some(playback_url) = source.playback_url.clone() else { return };
        let key = resolved_key(video, source);
        let fresh = self.sources.resolved.get(&key).is_some_and(|(at, _)| at.elapsed() < RESOLVED_FRESH);
        if fresh || self.sources.resolving_ahead.contains(&key) {
            return;
        }
        let Some(comet) = self.comet() else { return };
        self.sources.resolving_ahead.insert(key.clone());
        let started = Instant::now();
        self.spawn(async move { comet.resolve_playback(&playback_url).await }, move |c, result| {
            c.sources.resolving_ahead.remove(&key);
            match result {
                Ok(url) => {
                    tracing::info!("stream link ready ahead in {:.1} s", started.elapsed().as_secs_f64());
                    c.sources.resolved.retain(|_, (at, _)| at.elapsed() < RESOLVED_FRESH);
                    c.sources.resolved.insert(key, (Instant::now(), url));
                }
                Err(e) => tracing::info!("stream link ahead: {e}"),
            }
        });
    }

    /// A link resolved ahead for this stream, used once.
    fn take_resolved(&mut self, video: &VideoRef, source: &Source) -> Option<String> {
        let (at, url) = self.sources.resolved.remove(&resolved_key(video, source))?;
        (at.elapsed() < RESOLVED_FRESH).then_some(url)
    }

    // ---- the picker ----

    pub fn show_picker(&mut self, request: PlayRequest) {
        self.sources.generation += 1;
        let (title, subtitle) = super::video_title(&request.context.name, &request.video, request.context.episode_title.as_deref());
        let state = SourcesState::get(&self.ui);
        state.set_heading("Choose a source".into());
        state.set_subheading(shared(if subtitle.is_empty() { title } else { format!("{title}  ·  {subtitle}") }));
        state.set_message("".into());
        state.set_message_action("".into());
        state.set_footer("".into());
        state.set_sources(ModelRc::default());
        state.set_focused_index(0);
        state.set_open(true);
        self.sources.open = true;
        self.sources.ranked = None;
        self.sources.shown.clear();
        self.sources.show_all = false;
        self.sources.index = 0;
        self.sources.notices.clear();
        self.sources.key_problem = false;
        self.sources.can_retry = false;
        let video = request.video.clone();
        let id = video.video_id();
        self.sources.request = Some(request);

        // Usually fetched while the detail page was open.
        if let Some(found) = self.fresh_streams(&id) {
            self.sources.waiting = None;
            state.set_loading(false);
            self.sources_loaded(Ok(found));
            return;
        }
        state.set_loading(true);
        self.sources.waiting = Some(id);
        self.prefetch_streams(&video);
    }

    fn sources_loaded(&mut self, result: Result<CometResult, CoreError>) {
        SourcesState::get(&self.ui).set_loading(false);
        match result {
            Ok(result) => {
                self.sources.key_problem = result.notices.iter().any(|n| {
                    let n = n.to_ascii_lowercase();
                    n.contains("api key") || n.contains("token") || n.contains("premium")
                });
                self.sources.can_retry = result.sources.is_empty() && !self.sources.key_problem;
                self.sources.notices = result.notices;
                self.sources.ranked = Some(RankedSources::rank(result.sources, &self.settings.filters, &self.caps));
                self.push_sources();
            }
            Err(e) => {
                self.sources.can_retry = true;
                let state = SourcesState::get(&self.ui);
                state.set_message(shared(format!("Couldn't search for sources. {e}")));
                state.set_message_action("Try again".into());
            }
        }
    }

    fn push_sources(&mut self) {
        let Some(ranked) = self.sources.ranked.as_ref() else { return };
        let mut shown: Vec<(Source, Option<Hidden>)> = ranked.visible.iter().cloned().map(|s| (s, None)).collect();
        if self.sources.show_all {
            shown.extend(ranked.hidden.iter().cloned().map(|(s, h)| (s, Some(h))));
        }
        let items: Vec<SourceItem> = shown
            .iter()
            .map(|(s, hidden)| SourceItem {
                quality: shared(format::quality_label(s)),
                title: shared(&s.release.release_name),
                badges: ModelRc::new(VecModel::from(
                    s.release.badges().into_iter().take(6).map(slint::SharedString::from).collect::<Vec<_>>(),
                )),
                info: shared(format::source_info(s)),
                cached: s.cached,
                hidden_reason: shared(hidden.map(format::hidden_label).unwrap_or_default()),
            })
            .collect();
        let hidden = ranked.hidden.len();
        let footer = match (hidden, self.sources.show_all) {
            (0, _) => String::new(),
            (n, false) => format!("Show {n} hidden source{}", if n == 1 { "" } else { "s" }),
            (_, true) => "Hide filtered sources".to_owned(),
        };
        let message = if !self.sources.notices.is_empty() && shown.is_empty() {
            format!("Comet: {}", self.sources.notices.join(" "))
        } else if shown.is_empty() && hidden > 0 {
            format!("All {hidden} sources were hidden by your filters (this TV or your settings).")
        } else if shown.is_empty() {
            "No sources found for this title yet.".to_owned()
        } else {
            String::new()
        };
        let action = if !shown.is_empty() {
            ""
        } else if self.sources.key_problem {
            "Open Settings"
        } else if self.sources.can_retry {
            "Try again"
        } else {
            ""
        };
        let state = SourcesState::get(&self.ui);
        state.set_message(shared(message));
        state.set_message_action(action.into());
        state.set_footer(shared(footer));
        state.set_sources(ModelRc::new(VecModel::from(items)));
        self.sources.shown = shown;
        self.sources.index = self.sources.index.min(self.source_slots().saturating_sub(1));
        state.set_focused_index(self.sources.index as i32);
    }

    fn source_slots(&self) -> usize {
        let footer = self.sources.ranked.as_ref().is_some_and(|r| !r.hidden.is_empty());
        self.sources.shown.len() + usize::from(footer)
    }

    pub fn close_sources(&mut self) {
        self.sources.open = false;
        self.sources.waiting = None;
        self.sources.generation += 1;
        SourcesState::get(&self.ui).set_open(false);
    }

    pub fn sources_key(&mut self, key: Key) -> bool {
        let slots = self.source_slots();
        match key {
            Key::Back => self.close_sources(),
            Key::Up => self.sources.index = self.sources.index.saturating_sub(1),
            Key::Down => self.sources.index = (self.sources.index + 1).min(slots.saturating_sub(1)),
            Key::PageUp => self.sources.index = self.sources.index.saturating_sub(5),
            Key::PageDown => self.sources.index = (self.sources.index + 5).min(slots.saturating_sub(1)),
            Key::Ok => {
                if self.sources.shown.is_empty() && self.sources.key_problem {
                    self.close_sources();
                    self.detail = None;
                    self.go_section(Section::Settings);
                    return true;
                }
                if self.sources.shown.is_empty() && self.sources.can_retry && self.sources.waiting.is_none() {
                    if let Some(request) = self.sources.request.clone() {
                        self.sources.streams.remove(&request.video.video_id());
                        self.show_picker(request);
                    }
                    return true;
                }
                if self.sources.index == self.sources.shown.len() && slots > self.sources.shown.len() {
                    self.sources.show_all = !self.sources.show_all;
                    self.push_sources();
                    return true;
                }
                if let (Some((source, _)), Some(request)) =
                    (self.sources.shown.get(self.sources.index).cloned(), self.sources.request.clone())
                {
                    self.resolve(source, request, false);
                }
                return true;
            }
            _ => {}
        }
        SourcesState::get(&self.ui).set_focused_index(self.sources.index as i32);
        true
    }

    // ---- resolving ----

    pub(super) fn resolve(&mut self, source: Source, request: PlayRequest, direct: bool) {
        self.sources.generation += 1;
        let generation = self.sources.generation;
        self.sources.play_started.get_or_insert_with(Instant::now);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        self.sources.resolving = Some(ResolveCtl {
            cancel: cancel_tx,
            started: Instant::now(),
            downloading_since: None,
            request: request.clone(),
            source: source.clone(),
            error: None,
            direct,
            generation,
            status: if source.cached { "Getting the stream from Real-Debrid…".into() } else { "Sending to Real-Debrid…".into() },
            progress: -1.0,
            hint: String::new(),
        });
        self.push_start();

        // Resolved while the detail page was open, or during the previous episode.
        if let Some(url) = self.take_resolved(&request.video, &source) {
            tracing::info!("stream link was ready ahead");
            self.resolve_finished(generation, Ok(url));
            return;
        }
        let (Some(comet), Some(rd)) = (self.comet(), self.real_debrid()) else { return };
        let video = request.video.clone();
        let progress = move |p: ResolveProgress| {
            let _ = slint::invoke_from_event_loop(move || with_controller(|c| c.resolve_progress(generation, p)));
        };
        self.spawn(
            async move {
                // Cached: Comet's playback URL redirects straight to the stream.
                if source.cached
                    && let Some(url) = source.playback_url.as_deref()
                {
                    match comet.resolve_playback(url).await {
                        Ok(direct) => return Ok(direct),
                        Err(e) => tracing::warn!("Comet playback failed, resolving on the TV: {e}"),
                    }
                }
                rd.resolve(&source, &video, &progress, cancel_rx).await.map(|r| r.url)
            },
            move |c, result: Result<String, CoreError>| c.resolve_finished(generation, result),
        );
    }

    fn resolve_finished(&mut self, generation: u64, result: Result<String, CoreError>) {
        let Some(resolving) = self.sources.resolving.as_mut() else { return };
        if resolving.generation != generation {
            return;
        }
        match result {
            Ok(url) => {
                tracing::info!("stream resolved in {:.1} s", resolving.started.elapsed().as_secs_f64());
                let (request, source) = (resolving.request.clone(), resolving.source.clone());
                self.sources.resolving = None;
                self.close_sources();
                self.start_playback(request, source, url);
            }
            Err(CoreError::Cancelled) => {}
            Err(error) => {
                tracing::warn!("resolving the stream failed: {error}");
                let bad_key = matches!(&error, CoreError::RealDebrid(e) if e.kind == RdErrorKind::BadKey);
                if resolving.direct && resolving.request.fallback_to_picker && !bad_key {
                    // The remembered source failed: let the user pick another.
                    let mut request = resolving.request.clone();
                    request.preferred = None;
                    self.sources.resolving = None;
                    self.leave_player();
                    self.toast("That source isn't available anymore. Pick another one.");
                    self.show_picker(request);
                    self.push_start();
                    return;
                }
                resolving.error = Some(error.to_string());
                self.push_start();
            }
        }
    }

    fn resolve_progress(&mut self, generation: u64, progress: ResolveProgress) {
        let Some(resolving) = self.sources.resolving.as_mut() else { return };
        if resolving.generation != generation || resolving.error.is_some() {
            return;
        }
        match progress {
            ResolveProgress::Adding => resolving.status = "Sending to Real-Debrid…".into(),
            ResolveProgress::ReadingFiles => resolving.status = "Reading the torrent's files…".into(),
            ResolveProgress::Downloading { percent, speed, seeders, status } => {
                resolving.downloading_since.get_or_insert_with(Instant::now);
                let mut parts = vec![if status == "queued" {
                    "Queued on Real-Debrid".to_owned()
                } else {
                    format!("Real-Debrid is downloading · {percent:.0}%")
                }];
                if let Some(speed) = speed.filter(|s| *s > 0) {
                    parts.push(format!("{:.1} MB/s", speed as f64 / 1e6));
                }
                if let Some(seeders) = seeders {
                    parts.push(format!("{seeders} seeders"));
                }
                resolving.status = parts.join("  ·  ");
                resolving.progress = percent / 100.0;
            }
            ResolveProgress::Unrestricting => {
                resolving.status = "Almost ready…".into();
                resolving.progress = 1.0;
            }
        }
        self.push_start();
    }

    pub(super) fn resolve_tick(&mut self) {
        let Some(resolving) = self.sources.resolving.as_mut() else { return };
        if resolving.error.is_some() {
            return;
        }
        let waited = resolving.downloading_since.map(|t| t.elapsed()).unwrap_or_default();
        let hint = if waited > Duration::from_secs(10) {
            format!(
                "This file isn't cached yet, so it can take a while. You can follow it at {TORRENTS_URL} or press Back and pick an Instant source."
            )
        } else if resolving.started.elapsed() > Duration::from_secs(12) && resolving.downloading_since.is_none() {
            "Still waiting on Real-Debrid…".to_owned()
        } else {
            return;
        };
        if resolving.hint != hint {
            resolving.hint = hint;
            self.push_start();
        }
    }

    /// Keys while a stream resolves: Back cancels, OK retries after an error.
    pub fn resolve_key(&mut self, key: Key) -> bool {
        let failed = self.sources.resolving.as_ref().is_some_and(|r| r.error.is_some());
        match key {
            Key::Back | Key::Stop => {
                let Some(resolving) = self.sources.resolving.take() else { return true };
                let _ = resolving.cancel.send(true);
                self.sources.play_started = None;
                if resolving.direct && !self.sources.open {
                    // From Resume or the next episode.
                    self.leave_player();
                    if failed {
                        // Offer the picker so they can choose another source.
                        let mut request = resolving.request;
                        request.preferred = None;
                        self.show_picker(request);
                    }
                }
                self.push_start();
            }
            Key::Ok if failed => {
                if let Some(resolving) = self.sources.resolving.take() {
                    let _ = resolving.cancel.send(true);
                    self.resolve(resolving.source, resolving.request, resolving.direct);
                }
            }
            _ => {}
        }
        true
    }

    /// Close the player (nothing to save: it hasn't started) and go back to
    /// the page below it, e.g. to show the picker there.
    pub(super) fn leave_player(&mut self) {
        if self.screen == Screen::Player {
            self.stop_playback(false);
            self.pop_screen();
        }
    }
}
