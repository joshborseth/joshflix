//! The Stremio-style source picker and the "Getting your stream" step.

use std::time::{Duration, Instant};

use joshflix_core::CoreError;
use joshflix_core::comet::Comet;
use joshflix_core::realdebrid::{RdErrorKind, RealDebrid, ResolveProgress, TORRENTS_URL};
use joshflix_core::release::Hidden;
use joshflix_core::sources::{RankedSources, Source};
use slint::{Global, ModelRc, VecModel};
use tokio::sync::watch;

use super::detail::PlayRequest;
use super::{Controller, Key, Section, shared, with_controller};
use crate::format;
use crate::{SourceItem, SourcesState};

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
    pub resolving: Option<ResolveCtl>,
}

pub struct ResolveCtl {
    cancel: watch::Sender<bool>,
    started: Instant,
    downloading_since: Option<Instant>,
    request: PlayRequest,
    error: Option<String>,
    /// Came from Resume/next-episode without the picker.
    direct: bool,
    generation: u64,
}

impl Controller {
    pub(super) fn comet(&self) -> Option<Comet> {
        let key = self.rd_key()?;
        // Desktop development can point at a mock Comet.
        #[cfg(not(target_os = "android"))]
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

    pub fn show_picker(&mut self, request: PlayRequest) {
        self.sources.generation += 1;
        let generation = self.sources.generation;
        let (title, subtitle) = super::video_title(&request.context.name, &request.video, request.context.episode_title.as_deref());
        let state = SourcesState::get(&self.ui);
        state.set_heading("Choose a source".into());
        state.set_subheading(shared(if subtitle.is_empty() { title } else { format!("{title}  ·  {subtitle}") }));
        state.set_loading(true);
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
        let video = request.video.clone();
        self.sources.request = Some(request);

        let Some(comet) = self.comet() else { return };
        self.spawn(async move { comet.streams(&video).await }, move |c, result| {
            if c.sources.generation != generation || !c.sources.open {
                return;
            }
            SourcesState::get(&c.ui).set_loading(false);
            match result {
                Ok(result) => {
                    c.sources.key_problem = result.notices.iter().any(|n| {
                        let n = n.to_ascii_lowercase();
                        n.contains("api key") || n.contains("token") || n.contains("premium")
                    });
                    c.sources.notices = result.notices;
                    c.sources.ranked = Some(RankedSources::rank(result.sources, &c.settings.filters, &c.caps));
                    c.push_sources();
                }
                Err(e) => {
                    let state = SourcesState::get(&c.ui);
                    state.set_message(shared(format!("Couldn't search for sources. {e}")));
                }
            }
        });
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
            "No sources found for this title yet. Try again later.".to_owned()
        } else {
            String::new()
        };
        let state = SourcesState::get(&self.ui);
        state.set_message(shared(message));
        state.set_message_action(if self.sources.key_problem && shown.is_empty() { "Open Settings".into() } else { "".into() });
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

    fn resolve(&mut self, source: Source, request: PlayRequest, direct: bool) {
        self.sources.generation += 1;
        let generation = self.sources.generation;
        let (cancel_tx, cancel_rx) = watch::channel(false);
        self.sources.resolving = Some(ResolveCtl {
            cancel: cancel_tx,
            started: Instant::now(),
            downloading_since: None,
            request: request.clone(),
            error: None,
            direct,
            generation,
        });
        let state = SourcesState::get(&self.ui);
        state.set_resolving(true);
        state.set_resolve_title(shared(match request.video.episode_label() {
            Some(label) => format!("{}  ·  {label}", request.context.name),
            None => request.context.name.clone(),
        }));
        state.set_resolve_status(if source.cached { "Starting from Real-Debrid…".into() } else { "Sending to Real-Debrid…".into() });
        state.set_resolve_progress(-1.0);
        state.set_resolve_hint("".into());
        state.set_resolve_error("".into());

        let (Some(comet), Some(rd)) = (self.comet(), self.real_debrid()) else { return };
        let video = request.video.clone();
        let source_for_task = source.clone();
        let progress = move |p: ResolveProgress| {
            let _ = slint::invoke_from_event_loop(move || with_controller(|c| c.resolve_progress(generation, p)));
        };
        self.spawn(
            async move {
                // Cached: Comet's playback URL redirects straight to the stream.
                if source_for_task.cached
                    && let Some(url) = source_for_task.playback_url.as_deref()
                {
                    match comet.resolve_playback(url).await {
                        Ok(direct) => return Ok(direct),
                        Err(e) => tracing::warn!("Comet playback failed, resolving on the TV: {e}"),
                    }
                }
                rd.resolve(&source_for_task, &video, &progress, cancel_rx).await.map(|r| r.url)
            },
            move |c, result: Result<String, CoreError>| {
                let Some(resolving) = c.sources.resolving.as_mut() else { return };
                if resolving.generation != generation {
                    return;
                }
                match result {
                    Ok(url) => {
                        let request = resolving.request.clone();
                        c.sources.resolving = None;
                        SourcesState::get(&c.ui).set_resolving(false);
                        c.close_sources();
                        c.start_playback(request, source, url);
                    }
                    Err(CoreError::Cancelled) => {}
                    Err(error) => {
                        let bad_key = matches!(&error, CoreError::RealDebrid(e) if e.kind == RdErrorKind::BadKey);
                        if resolving.direct && resolving.request.fallback_to_picker && !bad_key {
                            // The remembered source failed: let the user pick another.
                            let mut request = resolving.request.clone();
                            request.preferred = None;
                            c.sources.resolving = None;
                            SourcesState::get(&c.ui).set_resolving(false);
                            c.toast("That source isn't available anymore. Pick another one.");
                            c.show_picker(request);
                            return;
                        }
                        resolving.error = Some(error.to_string());
                        SourcesState::get(&c.ui).set_resolve_error(shared(error.to_string()));
                    }
                }
            },
        );
    }

    fn resolve_progress(&mut self, generation: u64, progress: ResolveProgress) {
        let Some(resolving) = self.sources.resolving.as_mut() else { return };
        if resolving.generation != generation || resolving.error.is_some() {
            return;
        }
        let state = SourcesState::get(&self.ui);
        match progress {
            ResolveProgress::Adding => state.set_resolve_status("Sending to Real-Debrid…".into()),
            ResolveProgress::ReadingFiles => state.set_resolve_status("Reading the torrent's files…".into()),
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
                state.set_resolve_status(shared(parts.join("  ·  ")));
                state.set_resolve_progress(percent / 100.0);
            }
            ResolveProgress::Unrestricting => {
                state.set_resolve_status("Almost ready…".into());
                state.set_resolve_progress(1.0);
            }
        }
    }

    pub(super) fn resolve_tick(&mut self) {
        let Some(resolving) = self.sources.resolving.as_ref() else { return };
        if resolving.error.is_some() {
            return;
        }
        let waited = resolving.downloading_since.map(|t| t.elapsed()).unwrap_or_default();
        if waited > Duration::from_secs(10) {
            SourcesState::get(&self.ui).set_resolve_hint(shared(format!(
                "This file isn't cached yet, so it can take a while. You can follow it at {TORRENTS_URL} or press Back and pick an Instant source."
            )));
        } else if resolving.started.elapsed() > Duration::from_secs(12) && resolving.downloading_since.is_none() {
            SourcesState::get(&self.ui).set_resolve_hint("Still waiting on Real-Debrid…".into());
        }
    }

    pub fn resolve_key(&mut self, key: Key) -> bool {
        if key == Key::Back
            && let Some(resolving) = self.sources.resolving.take()
        {
            let _ = resolving.cancel.send(true);
            SourcesState::get(&self.ui).set_resolving(false);
            if resolving.direct && !self.sources.open {
                // From Resume: offer the picker so they can choose another source.
                let mut request = resolving.request;
                request.preferred = None;
                if resolving.error.is_some() {
                    self.show_picker(request);
                }
            }
        }
        true
    }
}
